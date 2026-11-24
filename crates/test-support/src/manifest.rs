//! Fixture encoder for the proposed version 1 Git generation manifest.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use gh_docs_core::git::{ObjectFormat, RefName, RepositoryId};
use gh_docs_storage::catalog::RefSnapshot;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub struct Observation<'a> {
    pub repository: RepositoryId,
    pub source: Uuid,
    pub format: ObjectFormat,
    pub policy: u64,
    pub default_ref: &'a RefName,
    pub refs: &'a [RefSnapshot],
    pub observed_at: &'a str,
}

pub struct Digests {
    pub logical: [u8; 32],
    pub observed: [u8; 32],
}

impl Observation<'_> {
    fn content(&self) -> Result<Value, &'static str> {
        let mut refs: Vec<_> = self.refs.iter().collect();
        refs.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        if refs.windows(2).any(|pair| pair[0].name == pair[1].name)
            || !refs.iter().any(|entry| &entry.name == self.default_ref)
        {
            return Err("invalid manifest ref map");
        }
        let refs: Vec<_> = refs
            .iter()
            .map(|entry| {
                json!([
                    STANDARD.encode(entry.name.bytes()),
                    entry.kind.name(),
                    entry.target.to_string(),
                    entry.peeled_commit.map(|oid| oid.to_string()),
                ])
            })
            .collect();
        Ok(json!([
            self.repository.to_string(),
            self.source.to_string(),
            self.format.name(),
            self.policy.to_string(),
            STANDARD.encode(self.default_ref.bytes()),
            refs,
        ]))
    }

    pub fn digests(&self) -> Result<Digests, &'static str> {
        let content = self.content()?;
        let logical = digest(b"gh-docs-manifest-content-v1\0", &content);
        let envelope = json!([
            "gh-docs-manifest",
            1,
            content,
            ["verified", "fixture_checks", self.observed_at]
        ]);
        let observed = digest(b"gh-docs-manifest-observation-v1\0", &envelope);
        Ok(Digests { logical, observed })
    }
}

fn digest(domain: &[u8], data: &Value) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(domain);
    hash.update(data.to_string());
    hash.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gh_docs_core::git::{ObjectType, Oid};

    #[test]
    fn golden_manifest_distinguishes_content_and_observation() {
        let name = RefName::new(b"refs/heads/main".to_vec()).unwrap();
        let oid = Oid::Sha1([0x12; 20]);
        let refs = [RefSnapshot {
            name: name.clone(),
            target: oid,
            kind: ObjectType::Commit,
            peeled_commit: Some(oid),
        }];
        let mut observation = Observation {
            repository: "11111111-1111-4111-8111-111111111111".parse().unwrap(),
            source: "22222222-2222-4222-8222-222222222222".parse().unwrap(),
            format: ObjectFormat::Sha1,
            policy: 1,
            default_ref: &name,
            refs: &refs,
            observed_at: "2026-01-01T00:00:00.000000Z",
        };
        let first = observation.digests().unwrap();
        assert_eq!(
            observation.content().unwrap().to_string(),
            concat!(
                "[\"11111111-1111-4111-8111-111111111111\",",
                "\"22222222-2222-4222-8222-222222222222\",\"sha1\",\"1\",\"cmVmcy9oZWFkcy9tYWlu\",",
                "[[\"cmVmcy9oZWFkcy9tYWlu\",\"commit\",\"1212121212121212121212121212121212121212\",",
                "\"1212121212121212121212121212121212121212\"]]]"
            )
        );
        observation.observed_at = "2026-01-02T00:00:00.000000Z";
        let second = observation.digests().unwrap();
        assert_eq!(first.logical, second.logical);
        assert_ne!(first.observed, second.observed);
        assert_eq!(
            hex(&first.logical),
            "8aafdcc231cbbd0907450ebba1600a9bccfea28d01b17c3cb15a92c85b876405"
        );
        assert_eq!(
            hex(&first.observed),
            "a7e75908a1b3cd2bed126985c614fffc93243826950bec21a2744716b27acdbb"
        );
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}
