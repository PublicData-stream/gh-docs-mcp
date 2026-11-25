use std::sync::Arc;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::hmac;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Error;

const DOMAIN: &[u8] = b"gh-docs-exploration-cursor-v1\0";
const ARGUMENTS: &[u8] = b"gh-docs-exploration-arguments-v1\0";
const TAG_BYTES: usize = 48;
const MAX_TOKEN: usize = 1024;
const TTL: u64 = 1800;

pub struct CursorKey {
    id: String,
    key: hmac::Key,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Scope {
    pub repository_id: String,
    pub generation_id: i64,
    pub commit_oid: Option<String>,
    pub sequence: i64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(super) enum Position {
    Repository(uuid::Uuid),
    Offset(u64),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    v: u8,
    kid: String,
    tool: String,
    binding: String,
    scope: Option<Scope>,
    position: Position,
    expiry: u64,
}

pub(super) struct Pagination {
    key: Arc<CursorKey>,
    tool: &'static str,
    binding: String,
    expiry: u64,
    pub scope: Option<Scope>,
    pub position: Option<Position>,
}

impl CursorKey {
    pub fn new(id: String, secret: &[u8]) -> Result<Self, Error> {
        if id.is_empty()
            || id.len() > 32
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            || secret.len() != TAG_BYTES
        {
            return Err(Error::Internal);
        }
        Ok(Self {
            id,
            key: hmac::Key::new(hmac::HMAC_SHA384, secret),
        })
    }

    fn binding(&self, args: &Value) -> Result<String, Error> {
        let bytes = serde_json::to_vec(args).map_err(|_| Error::Internal)?;
        let mut mac = hmac::Context::with_key(&self.key);
        mac.update(ARGUMENTS);
        mac.update(&bytes);
        Ok(URL_SAFE_NO_PAD.encode(mac.sign()))
    }
}

impl Pagination {
    pub fn open(
        key: &Arc<CursorKey>,
        tool: &'static str,
        args: &Value,
        token: Option<&str>,
        now: u64,
    ) -> Result<Self, Error> {
        let binding = key.binding(args)?;
        let Some(token) = token else {
            return Ok(Self {
                key: Arc::clone(key),
                tool,
                binding,
                expiry: now + TTL,
                scope: None,
                position: None,
            });
        };
        if token.len() > MAX_TOKEN {
            return Err(Error::CursorInvalid);
        }
        let mut signed = DOMAIN.to_vec();
        URL_SAFE_NO_PAD
            .decode_vec(token, &mut signed)
            .map_err(|_| Error::CursorInvalid)?;
        let split = signed
            .len()
            .checked_sub(TAG_BYTES)
            .filter(|&split| split > DOMAIN.len())
            .ok_or(Error::CursorInvalid)?;
        let (message, mac) = signed.split_at(split);
        hmac::verify(&key.key, message, mac).map_err(|_| Error::CursorInvalid)?;
        let payload: Payload =
            serde_json::from_slice(&message[DOMAIN.len()..]).map_err(|_| Error::CursorInvalid)?;
        if payload.v != 1
            || payload.kid != key.id
            || payload.tool != tool
            || payload.binding != binding
            || payload.expiry > now + TTL
        {
            return Err(Error::CursorInvalid);
        }
        if payload.expiry <= now {
            return Err(Error::CursorExpired);
        }
        Ok(Self {
            key: Arc::clone(key),
            tool,
            binding,
            expiry: payload.expiry,
            scope: payload.scope,
            position: Some(payload.position),
        })
    }

    pub fn encode(&self, scope: Option<Scope>, position: Position) -> Result<String, Error> {
        let mut signed = DOMAIN.to_vec();
        serde_json::to_writer(
            &mut signed,
            &Payload {
                v: 1,
                kid: self.key.id.clone(),
                tool: self.tool.into(),
                binding: self.binding.clone(),
                scope,
                position,
                expiry: self.expiry,
            },
        )
        .map_err(|_| Error::Internal)?;
        let mac = hmac::sign(&self.key.key, &signed);
        signed.extend_from_slice(mac.as_ref());
        let token = URL_SAFE_NO_PAD.encode(&signed[DOMAIN.len()..]);
        if token.len() > MAX_TOKEN {
            return Err(Error::Internal);
        }
        Ok(token)
    }

    pub fn offset(&self) -> Result<Option<u64>, Error> {
        match (&self.position, &self.scope) {
            (None, None) => Ok(None),
            (Some(Position::Offset(offset)), Some(_)) => Ok(Some(*offset)),
            _ => Err(Error::CursorInvalid),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cursor_authentication_binding_and_original_expiry() {
        let key = Arc::new(CursorKey::new("test".into(), &[42; 48]).unwrap());
        let args = json!({"query":"private"});
        let first = Pagination::open(&key, "list", &args, None, 100).unwrap();
        let token = first
            .encode(None, Position::Repository(uuid::Uuid::nil()))
            .unwrap();
        let second = Pagination::open(&key, "list", &args, Some(&token), 1300).unwrap();
        assert_eq!(second.expiry, 1900);
        let second_token = second
            .encode(None, Position::Repository(uuid::Uuid::nil()))
            .unwrap();
        assert_eq!(
            Pagination::open(&key, "list", &args, Some(&second_token), 1900).err(),
            Some(Error::CursorExpired),
        );
        assert_eq!(
            Pagination::open(&key, "find", &args, Some(&token), 100).err(),
            Some(Error::CursorInvalid),
        );
        assert_eq!(
            Pagination::open(&key, "list", &json!({}), Some(&token), 100).err(),
            Some(Error::CursorInvalid),
        );
        let mut raw = URL_SAFE_NO_PAD.decode(&token).unwrap();
        assert!(!raw.windows(7).any(|part| part == b"private"));
        raw[5] ^= 1;
        assert_eq!(
            Pagination::open(&key, "list", &args, Some(&URL_SAFE_NO_PAD.encode(raw)), 100).err(),
            Some(Error::CursorInvalid),
        );
    }
}
