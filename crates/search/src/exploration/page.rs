use serde_json::{Value, json};

use super::{
    Error, RESPONSE_BYTES,
    cursor::{Pagination, Position, Scope},
};

pub(super) struct Entries {
    limit: usize,
    after: u64,
    seen: u64,
    used: usize,
    results: Vec<Value>,
    more: bool,
}

impl Entries {
    pub fn new(limit: usize, after: Option<u64>, base: &Value) -> Result<Self, Error> {
        Ok(Self {
            limit,
            after: after.unwrap_or(0),
            seen: 0,
            used: json_size(base)? + 2048,
            results: Vec::new(),
            more: false,
        })
    }

    /// Stop only after confirming another approved entry exists.
    pub fn push(&mut self, value: Value) -> Result<bool, Error> {
        self.seen += 1;
        if self.seen <= self.after {
            return Ok(true);
        }
        let size = json_size(&value)? + 1;
        if self.used + size > RESPONSE_BYTES || self.results.len() == self.limit {
            if self.results.is_empty() {
                return Err(Error::Budget);
            }
            self.more = true;
            return Ok(false);
        }
        self.used += size;
        self.results.push(value);
        Ok(true)
    }

    pub fn finish(
        self,
        mut base: Value,
        pagination: &Pagination,
        scope: Option<Scope>,
        budget: bool,
        last_repository: impl FnOnce(&Value) -> Result<uuid::Uuid, Error>,
    ) -> Result<Value, Error> {
        let count = self.results.len();
        let cursor = if self.more {
            let position = if scope.is_some() {
                Position::Offset(self.after + count as u64)
            } else {
                Position::Repository(last_repository(
                    self.results.last().ok_or(Error::Internal)?,
                )?)
            };
            Some(pagination.encode(scope, position)?)
        } else {
            None
        };
        base["coverage"] = coverage(!self.more && !budget, budget, count, None);
        base["entries"] = json!(self.results);
        base["next_cursor"] = json!(cursor);
        Ok(base)
    }
}

pub(super) fn coverage(complete: bool, budget: bool, count: usize, bytes: Option<usize>) -> Value {
    json!({
        "complete":complete,
        "returned_count":count,
        "returned_bytes":bytes,
        "reason": if budget { Some("narrow_scope") } else if !complete { Some("next_page") } else { None },
    })
}

pub(super) fn json_size(value: &Value) -> Result<usize, Error> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .map_err(|_| Error::Internal)
}
