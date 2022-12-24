use crate::db::QueryResult;
use crate::error::Result;
use crate::event::{single_char_tagname, Event};
use crate::nip05::VerificationRecord;
use crate::repo::{common, NostrRepo, PostgresPool};
use crate::subscription::{ReqFilter, Subscription};
use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use sqlx::{Postgres, QueryBuilder};

use crate::repo::postgres_migration::run_migrations;
use crate::utils::{is_hex, is_lower_hex};
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot::Receiver;
use tracing::info;
use crate::hexrange::{hex_range, HexSearch};

pub struct PostgresRepo {
    conn: PostgresPool,
}

impl PostgresRepo {
    pub fn new(c: PostgresPool) -> PostgresRepo {
        PostgresRepo { conn: c }
    }
}

#[async_trait]
impl NostrRepo for PostgresRepo {
    async fn migrate_up(&self) -> Result<usize> {
        run_migrations(&self.conn).await
    }

    async fn write_event(&self, e: &Event) -> Result<u64> {
        // start transaction
        let mut tx = self.conn.begin().await?;

        // get relevant fields from event and convert to blobs.
        let id_blob = hex::decode(&e.id).ok();
        let pubkey_blob: Option<Vec<u8>> = hex::decode(&e.pubkey).ok();
        let delegator_blob: Option<Vec<u8>> =
            e.delegated_by.as_ref().and_then(|d| hex::decode(d).ok());
        let event_str = serde_json::to_string(&e).unwrap();

        // ignore if the event hash is a duplicate.
        let mut ins_count = sqlx::query(
            r#"INSERT INTO "event"
(id, pub_key, created_at, kind, "content", delegated_by)
VALUES($1, $2, $3, $4, $5, $6)
ON CONFLICT (id) DO NOTHING"#,
        )
        .bind(&id_blob)
        .bind(&pubkey_blob)
        .bind(Utc.timestamp_opt(e.created_at as i64, 0).unwrap())
        .bind(e.kind as i64)
        .bind(event_str.into_bytes())
        .bind(delegator_blob)
        .execute(&mut tx)
        .await?
        .rows_affected();

        if ins_count == 0 {
            // if the event was a duplicate, no need to insert event or
            // pubkey references.  This will abort the txn.
            return Ok(0);
        }

        // add all tags to the tag table
        for tag in e.tags.iter() {
            // ensure we have 2 values.
            if tag.len() >= 2 {
                let tag_name = &tag[0];
                let tag_val = &tag[1];
                // only single-char tags are searchable
                let tag_char_opt = single_char_tagname(tag_name);
                let query = "INSERT INTO tag (event_id, \"name\", value) VALUES($1, $2, $3) ON CONFLICT (event_id, \"name\") DO NOTHING";
                match &tag_char_opt {
                    Some(_) => {
                        // if tag value is lowercase hex;
                        if is_lower_hex(tag_val) && (tag_val.len() % 2 == 0) {
                            sqlx::query(query)
                                .bind(&id_blob)
                                .bind(tag_name)
                                .bind(hex::decode(tag_val).ok())
                                .execute(&mut tx)
                                .await?;
                        } else {
                            sqlx::query(query)
                                .bind(&id_blob)
                                .bind(tag_name)
                                .bind(tag_val)
                                .execute(&mut tx)
                                .await?;
                        }
                    }
                    None => {}
                }
            }
        }

        // if this event is replaceable update, hide every other replaceable
        // event with the same kind from the same author that was issued
        // earlier than this.
        if e.kind == 0 || e.kind == 3 || (e.kind >= 10000 && e.kind < 20000) {
            let update_count = sqlx::query("UPDATE \"event\" SET hidden = 1::bit(1) \
            WHERE id != $1 AND kind = $2 AND pub_key = $3 AND created_at <= $4 and hidden != 1::bit(1)")
                .bind(&id_blob)
                .bind(e.kind as i64)
                .bind(hex::decode(&e.pubkey).ok())
                .bind(e.created_at as i64)
                .execute(&mut tx)
                .await?
                .rows_affected();
            if update_count > 0 {
                info!(
                    "hid {} older replaceable kind {} events for author: {:?}",
                    update_count,
                    e.kind,
                    e.get_author_prefix()
                );
            }
        }

        // if this event is a deletion, hide the referenced events from the same author.
        if e.kind == 5 {
            let event_candidates = e.tag_values_by_name("e");
            let pub_keys: Vec<Vec<u8>> = event_candidates
                .iter()
                .filter(|x| is_hex(x) && x.len() == 64)
                .filter_map(|x| hex::decode(x).ok())
                .collect();

            let mut builder = QueryBuilder::new(
                "UPDATE \"event\" SET hidden = 1::bit(1) WHERE kind != 5 AND pub_key = ",
            );
            builder.push_bind(hex::decode(&e.pubkey).ok());
            builder.push(" AND event_hash IN (");

            let mut sep = builder.separated(", ");
            for pk in pub_keys {
                sep.push_bind(pk);
            }
            sep.push_unseparated(")");

            let update_count = builder.build().execute(&mut tx).await?.rows_affected();
            info!(
                "hid {} deleted events for author {:?}",
                update_count,
                e.get_author_prefix()
            );
        } else {
            // check if a deletion has already been recorded for this event.
            // Only relevant for non-deletion events
            let del_count = sqlx::query(
                "SELECT e.id FROM \"event\" e \
            LEFT JOIN tag t ON e.id = t.event_id \
            WHERE e.pub_key = $1 AND t.\"name\" = 'e' AND e.kind = 5 AND t.value = $2 LIMIT 1",
            )
            .bind(&pubkey_blob)
            .bind(&id_blob)
            .fetch_optional(&mut tx)
            .await?;

            // check if a the query returned a result, meaning we should
            // hid the current event
            if del_count.is_some() {
                // a deletion already existed, mark original event as hidden.
                info!(
                    "hid event: {:?} due to existing deletion by author: {:?}",
                    e.get_event_id_prefix(),
                    e.get_author_prefix()
                );
                sqlx::query("UPDATE \"event\" SET hidden = 1::bit(1) WHERE id = $1")
                    .bind(&id_blob)
                    .execute(&mut tx)
                    .await?;
                // event was deleted, so let caller know nothing new
                // arrived, preventing this from being sent to active
                // subscriptions
                ins_count = 0;
            }
        }
        tx.commit().await?;
        Ok(ins_count)
    }

    async fn query_subscription(
        &self,
        sub: Subscription,
        client_id: String,
        query_tx: Sender<QueryResult>,
        abandon_query_rx: Receiver<()>,
    ) -> Result<()> {
        common::query_sub(sub, client_id, query_tx, abandon_query_rx, query_from_filter, &self.conn).await
    }

    async fn optimize_db(&self) -> Result<()> {
        // Not implemented
        Ok(())
    }

    async fn create_verification_record(&self, event_id: &str, name: &str) -> Result<()> {
        todo!()
    }

    async fn update_verification_timestamp(&self, id: u64) -> Result<()> {
        todo!()
    }

    async fn fail_verification(&self, id: u64) -> Result<()> {
        todo!()
    }

    async fn delete_verification(&self, id: u64) -> Result<()> {
        todo!()
    }

    async fn get_latest_user_verification(&self, pub_key: &str) -> Result<VerificationRecord> {
        todo!()
    }

    async fn get_oldest_user_verification(&self, before: u64) -> Result<VerificationRecord> {
        todo!()
    }
}


/// Create a dynamic SQL query and params from a subscription filter.
fn query_from_filter(f: &ReqFilter) -> Option<QueryBuilder<Postgres>> {
    // if the filter is malformed, don't return anything.
    if f.force_no_match {
        return None;
    }

    let mut query = QueryBuilder::new("SELECT e.\"content\", e.created_at FROM \"event\" e WHERE ");

    let mut push_and = false;
    // Query for "authors", allowing prefix matches
    if let Some(auth_vec) = &f.authors {
        let mut range_authors = query.separated(" OR ");
        for auth in auth_vec {
            match hex_range(auth) {
                Some(HexSearch::Exact(ex)) => {
                    range_authors
                        .push("(e.pub_key = ")
                        .push_bind_unseparated(ex.clone())
                        .push_unseparated(" OR e.delegated_by = ")
                        .push_bind_unseparated(ex)
                        .push_unseparated(")");
                }
                Some(HexSearch::Range(lower, upper)) => {
                    range_authors
                        .push("(e.pub_key > ")
                        .push_bind_unseparated(lower.clone())
                        .push_unseparated(" AND e.pub_key < ")
                        .push_bind_unseparated(upper.clone())
                        .push_unseparated(" OR (e.delegated_by > ")
                        .push_bind_unseparated(lower)
                        .push_unseparated(" AND e.delegated_by < ")
                        .push_bind_unseparated(upper)
                        .push_unseparated(")");
                }
                Some(HexSearch::LowerOnly(lower)) => {
                    range_authors
                        .push("(e.pub_key > ")
                        .push_bind_unseparated(lower.clone())
                        .push_unseparated(" OR e.delegated_by > ")
                        .push_bind_unseparated(lower)
                        .push_unseparated(")");
                }
                None => {
                    info!("Could not parse hex range from author {:?}", auth);
                }
            }
            push_and = true;
        }
    }

    // Query for Kind
    if let Some(ks) = &f.kinds {
        if !ks.is_empty() {
            if push_and {
                query.push(" AND ");
            }
            push_and = true;
            // kind is number, no escaping needed
            let str_kinds: Vec<String> = ks.iter().map(|x| x.to_string()).collect();

            query.push("e.kind in (");
            let mut list_query = query.separated(", ");
            for k in str_kinds {
                list_query.push_bind(k);
            }
            query.push(")");
        }
    }

    // Query for event, allowing prefix matches
    if let Some(id_vec) = &f.ids {
        if !id_vec.is_empty() {
            if push_and {
                query.push(" AND ");
            }
            push_and = true;

            // take each author and convert to a hex search
            let mut id_query = query.separated(" OR ");
            for id in id_vec {
                match hex_range(id) {
                    Some(HexSearch::Exact(ex)) => {
                        id_query
                            .push("(id = ")
                            .push_bind_unseparated(ex)
                            .push_unseparated(")");
                    }
                    Some(HexSearch::Range(lower, upper)) => {
                        id_query
                            .push("(id > ")
                            .push_bind_unseparated(lower)
                            .push_unseparated(" AND id < ")
                            .push_bind_unseparated(upper)
                            .push_unseparated(")");
                    }
                    Some(HexSearch::LowerOnly(lower)) => {
                        id_query
                            .push("(id > ")
                            .push_bind_unseparated(lower)
                            .push_unseparated(")");
                    }
                    None => {
                        info!("Could not parse hex range from id {:?}", id);
                    }
                }
            }
        }
    }

    // Query for tags
    if let Some(map) = &f.tags {
        if !map.is_empty() {
            if push_and {
                query.push(" AND ");
            }
            push_and = true;

            for (key, val) in map.iter() {
                query.push("e.id IN (SELECT ee.id FROM \"event\" ee LEFT JOIN tag t on ee.id = t.event_id WHERE ee.hidden != 1::bit(1) and (t.\"name\" = ")
                    .push_bind(key.to_string())
                    .push(" AND (value in (");

                // plain value match first
                let mut tag_query = query.separated(", ");
                for v in val
                    .iter()
                {
                    if (v.len() % 2 != 0) && !is_lower_hex(v) {
                        tag_query.push_bind(v);
                    } else {
                        tag_query.push_bind(hex::decode(v).ok());
                    }
                }
                query.push(")))");
            }
        }
    }

    // Query for timestamp
    if f.since.is_some() {
        if push_and {
            query.push(" AND ");
        }
        push_and = true;
        query
            .push("e.created_at > ")
            .push_bind(Utc.timestamp_opt(f.since.unwrap() as i64, 0).unwrap());
    }

    // Query for timestamp
    if f.until.is_some() {
        if push_and {
            query.push(" AND ");
        }
        push_and = true;
        query
            .push("e.created_at < ")
            .push_bind(Utc.timestamp_opt(f.until.unwrap() as i64, 0).unwrap());
    }

    // never display hidden events
    if push_and {
        query.push(" AND hidden != 1::bit(1)");
    } else {
        query.push("hidden != 1::bit(1)");
    }

    // Apply per-filter limit to this query.
    // The use of a LIMIT implies a DESC order, to capture only the most recent events.
    if let Some(lim) = f.limit {
        query.push(" ORDER BY e.created_at DESC LIMIT ");
        query.push(lim);
    } else {
        query.push(" ORDER BY e.created_at ASC");
    }
    Some(query)
}