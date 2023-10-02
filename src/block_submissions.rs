use std::{collections::HashMap, io::Write};

use anyhow::Result;
use bytes::Bytes;
use chrono::{Datelike, Timelike};
use flate2::{write::GzEncoder, Compression};
use fred::{
    prelude::{RedisError, RedisErrorKind},
    types::{FromRedis, MultipleOrderedPairs, RedisKey, RedisMap, RedisValue},
};
use object_store::path::Path;
use serde::{Deserialize, Serialize};
use tokio::task::spawn_blocking;
use tracing::{debug, instrument};

use crate::units::Slot;

fn deserialize_eligible_at<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let eligible_at: Option<i64> = Option::deserialize(deserializer)?;

    // eligible_at used to be sent as a negative number instead of nil when not available.
    // Until we update our example submissions we handle this special bug case.
    let eligible_at = eligible_at.and_then(|eligible_at| {
        if eligible_at < 0 {
            None
        } else {
            Some(eligible_at as u64)
        }
    });

    Ok(eligible_at)
}

/// Block submission archive entries.
/// These are block submissions as they came in on the relay, plus some metadata.
#[derive(Deserialize, Serialize)]
pub struct BlockSubmission {
    builder_ip: Option<String>,
    // Not every block submission becomes eligible, so this field is optional.
    #[serde(deserialize_with = "deserialize_eligible_at")]
    eligible_at: Option<u64>,
    execution_payload_size: Option<u64>,
    http_encoding: Option<String>,
    pub payload: serde_json::Value,
    payload_encoding: String,
    received_at: u64,
    safe_to_propose: bool,
    sim_optimistic: Option<bool>,
    sim_request_error: Option<String>,
    sim_validation_error: Option<String>,
    sim_was_simulated: bool,
    // A status code is not always available. Both historically, and because builder-api doesn't
    // always provide one.
    status_code: Option<u16>,
    user_agent: Option<String>,
}

impl std::fmt::Debug for BlockSubmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state_root = self.state_root();
        f.debug_struct("BlockSubmission")
            .field("builder_ip", &self.builder_ip)
            .field("eligible_at", &self.eligible_at)
            .field("execution_payload_size", &self.execution_payload_size)
            .field("http_encoding", &self.http_encoding)
            .field("payload", &format!("<PAYLOAD_JSON:{state_root}>"))
            .field("payload_encoding", &self.payload_encoding)
            .field("received_at", &self.received_at)
            .field("safe_to_propose", &self.safe_to_propose)
            .field("sim_optimistic", &self.sim_optimistic)
            .field("sim_request_error", &self.sim_request_error)
            .field("sim_validation_error", &self.sim_validation_error)
            .field("sim_was_simulated", &self.sim_was_simulated)
            .field("status_code", &self.status_code)
            .field("user_agent", &self.user_agent)
            .finish()
    }
}

impl From<BlockSubmission> for MultipleOrderedPairs {
    fn from(entry: BlockSubmission) -> Self {
        let mut pairs: Vec<(RedisKey, RedisValue)> = vec![
            (
                "payload".into(),
                RedisValue::String(serde_json::to_string(&entry.payload).unwrap().into()),
            ),
            (
                "received_at".into(),
                RedisValue::Integer(entry.received_at.try_into().unwrap()),
            ),
            (
                "safe_to_propose".into(),
                RedisValue::Boolean(entry.safe_to_propose),
            ),
            (
                "sim_was_simulated".into(),
                RedisValue::Boolean(entry.sim_was_simulated),
            ),
            (
                "payload_encoding".into(),
                RedisValue::String(entry.payload_encoding.into()),
            ),
        ];

        if let Some(eligible_at) = entry.eligible_at {
            pairs.push((
                "eligible_at".into(),
                RedisValue::Integer(eligible_at as i64),
            ))
        }

        if let Some(status_code) = entry.status_code {
            pairs.push((
                "status_code".into(),
                RedisValue::Integer(status_code.into()),
            ))
        }

        if let Some(sim_request_error) = entry.sim_request_error {
            pairs.push(("sim_request_error".into(), sim_request_error.into()))
        }

        if let Some(sim_validation_error) = entry.sim_validation_error {
            pairs.push(("sim_validation_error".into(), sim_validation_error.into()))
        }

        if let Some(sim_optimistic) = entry.sim_optimistic {
            pairs.push(("sim_optimistic".into(), RedisValue::Boolean(sim_optimistic)))
        }

        if let Some(user_agent) = entry.user_agent {
            pairs.push(("user_agent".into(), user_agent.into()))
        }

        if let Some(builder_ip) = entry.builder_ip {
            pairs.push(("builder_ip".into(), builder_ip.into()))
        }

        if let Some(execution_payload_size) = entry.execution_payload_size {
            pairs.push((
                "execution_payload_size".into(),
                RedisValue::Integer(execution_payload_size as i64),
            ))
        }

        if let Some(http_encoding) = entry.http_encoding {
            pairs.push(("http_encoding".into(), http_encoding.into()))
        }

        pairs.try_into().unwrap()
    }
}

fn into_redis_parse_err(err: impl std::fmt::Display) -> RedisError {
    RedisError::new(RedisErrorKind::Parse, err.to_string())
}

fn parse_string_optional(
    map: &'_ mut HashMap<String, RedisValue>,
    key: &str,
) -> Result<Option<String>, RedisError> {
    map.remove(key)
        .map(|rv| {
            rv.as_string()
                .ok_or_else(|| into_redis_parse_err(format!("failed to parse {} as string", key)))
        })
        .transpose()
}

fn parse_string_required(
    map: &'_ mut HashMap<String, RedisValue>,
    key: &str,
) -> Result<String, RedisError> {
    let v = map
        .remove(key)
        .ok_or_else(|| into_redis_parse_err(format!("expected {} in block submission", key)))?
        .as_string()
        .ok_or_else(|| {
            into_redis_parse_err(format!(
                "failed to parse {} as string, str: {}",
                key,
                map.get(key).unwrap().as_str().unwrap()
            ))
        })?;

    Ok(v)
}

fn parse_u64_optional(
    map: &'_ mut HashMap<String, RedisValue>,
    key: &str,
) -> Result<Option<u64>, RedisError> {
    map.remove(key)
        .map(|rv| {
            rv.as_u64().ok_or_else(|| {
                into_redis_parse_err(format!(
                    "failed to parse {} as u64, str: {}",
                    key,
                    rv.as_str().unwrap()
                ))
            })
        })
        .transpose()
}

fn parse_u64_required(
    map: &'_ mut HashMap<String, RedisValue>,
    key: &str,
) -> Result<u64, RedisError> {
    let v = map
        .remove(key)
        .ok_or_else(|| into_redis_parse_err(format!("expected {} in block submission", key)))?
        .as_u64()
        .ok_or_else(|| {
            into_redis_parse_err(format!(
                "failed to parse {} as u64, str: {}",
                key,
                map.get(key).unwrap().as_str().unwrap()
            ))
        })?;

    Ok(v)
}

fn parse_bool_optional(
    map: &'_ mut HashMap<String, RedisValue>,
    key: &str,
) -> Result<Option<bool>, RedisError> {
    map.remove(key)
        .map(|rv| {
            rv.as_bool().ok_or_else(|| {
                into_redis_parse_err(format!(
                    "failed to parse {} as bool, str: {}",
                    key,
                    rv.as_str().unwrap()
                ))
            })
        })
        .transpose()
}

fn parse_bool_required(
    map: &'_ mut HashMap<String, RedisValue>,
    key: &str,
) -> Result<bool, RedisError> {
    let v = map
        .remove(key)
        .ok_or_else(|| into_redis_parse_err(format!("expected {} in block submission", key)))?
        .as_bool()
        .ok_or_else(|| {
            into_redis_parse_err(format!(
                "failed to parse {} as bool, str: {}",
                key,
                map.get(key).unwrap().as_str().unwrap()
            ))
        })?;

    Ok(v)
}

impl FromRedis for BlockSubmission {
    fn from_value(value: RedisValue) -> Result<Self, RedisError> {
        let mut map: HashMap<String, RedisValue> = value.convert()?;

        let builder_ip = parse_string_optional(&mut map, "builder_ip")?;
        let eligible_at = parse_u64_optional(&mut map, "eligible_at")?;
        let execution_payload_size = parse_u64_optional(&mut map, "execution_payload_size")?;
        let http_encoding = parse_string_optional(&mut map, "http_encoding")?;
        let payload_encoding = parse_string_required(&mut map, "payload_encoding")?;
        let received_at = parse_u64_required(&mut map, "received_at")?;
        let safe_to_propose = parse_bool_required(&mut map, "safe_to_propose")?;
        let sim_optimistic = parse_bool_optional(&mut map, "sim_optimistic")?;
        let sim_request_error = parse_string_optional(&mut map, "sim_request_error")?;
        let sim_validation_error = parse_string_optional(&mut map, "sim_validation_error")?;
        let sim_was_simulated = parse_bool_required(&mut map, "sim_was_simulated")?;
        let status_code = parse_u64_optional(&mut map, "status_code")?.map(|v| v as u16);
        let user_agent = parse_string_optional(&mut map, "user_agent")?;

        let payload = {
            let str = map
                .remove("payload")
                .ok_or_else(|| into_redis_parse_err("expected payload in block submission"))?
                .as_bytes_str()
                .ok_or_else(|| into_redis_parse_err("failed to parse payload as bytes str"))?;
            // We could implement custom Deserialize for this to avoid parsing the JSON here, we
            // don't do anything with it besides Serialize it later.
            serde_json::from_str(&str).map_err(|e| {
                into_redis_parse_err(format!(
                    "failed to parse payload bytes as serde_json Value, {}",
                    e
                ))
            })?
        };

        Ok(Self {
            builder_ip,
            eligible_at,
            execution_payload_size,
            http_encoding,
            payload,
            payload_encoding,
            received_at,
            safe_to_propose,
            sim_optimistic,
            sim_request_error,
            sim_validation_error,
            sim_was_simulated,
            status_code,
            user_agent,
        })
    }
}

impl From<BlockSubmission> for RedisValue {
    fn from(entry: BlockSubmission) -> Self {
        #![allow(clippy::mutable_key_type)]
        let mut map: HashMap<RedisKey, RedisValue> = HashMap::new();
        if let Some(eligible_at) = entry.eligible_at {
            map.insert(
                "eligible_at".into(),
                RedisValue::Integer(eligible_at as i64),
            );
        }
        map.insert(
            "payload".into(),
            RedisValue::String(entry.payload.to_string().into()),
        );
        map.insert(
            "received_at".into(),
            RedisValue::Integer(entry.received_at as i64),
        );
        if let Some(status_code) = entry.status_code {
            map.insert(
                "status_code".into(),
                RedisValue::Integer(status_code.into()),
            );
        }
        if let Some(sim_optimistic) = entry.sim_optimistic {
            map.insert("sim_optimistic".into(), RedisValue::Boolean(sim_optimistic));
        }
        if let Some(sim_request_error) = entry.sim_request_error {
            map.insert("sim_request_error".into(), sim_request_error.into());
        }
        if let Some(sim_validation_error) = entry.sim_validation_error {
            map.insert("sim_validation_error".into(), sim_validation_error.into());
        }
        map.insert(
            "sim_was_simulated".into(),
            RedisValue::Boolean(entry.sim_was_simulated),
        );
        RedisMap::try_from(map).map(RedisValue::Map).unwrap()
    }
}

impl Default for BlockSubmission {
    fn default() -> Self {
        Self {
            builder_ip: None,
            eligible_at: None,
            execution_payload_size: None,
            http_encoding: None,
            payload: serde_json::Value::Null,
            payload_encoding: "json".to_string(),
            received_at: 0,
            safe_to_propose: false,
            sim_optimistic: None,
            sim_request_error: None,
            sim_validation_error: None,
            sim_was_simulated: false,
            status_code: None,
            user_agent: None,
        }
    }
}

impl BlockSubmission {
    pub fn bundle_path(&self) -> Path {
        let state_root = self.state_root();

        let slot = self.slot();
        let slot_date_time = slot.date_time();
        let year = slot_date_time.year();
        let month = slot_date_time.month();
        let day = slot_date_time.day();
        let hour = slot_date_time.hour();
        let minute = slot_date_time.minute();

        let path_string =
            format!("{year}/{month:02}/{day:02}/{hour:02}/{minute:02}/{slot}/{state_root}.json.gz");
        Path::from(path_string)
    }

    #[instrument(skip(self), fields(slot = %self.slot(), state_root = %self.state_root()))]
    pub async fn compress(&self) -> Result<Bytes> {
        let slot = self.slot();
        let state_root = self.state_root();

        let json_str = serde_json::to_string(&self)?;
        let json_size_kb = json_str.len() / 1000;

        let json_gz: Bytes = spawn_blocking(move || {
            let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(json_str.as_bytes())?;
            let json_gz = encoder.finish()?.into();
            Ok::<_, RedisError>(json_gz)
        })
        .await??;
        let json_gz_size_kb = json_gz.len() / 1000;

        let compression_ratio_truncated =
            ((json_size_kb as f64 / json_gz_size_kb as f64) * 100.0).trunc() / 100.0;

        debug!(
            slot = slot.to_string().as_str(),
            state_root = state_root.as_str(),
            uncompressed_size_kb = json_size_kb,
            compressed_size_kb = json_gz_size_kb,
            compression_ratio = compression_ratio_truncated,
            "compressed block submission"
        );

        Ok(json_gz)
    }

    pub fn slot(&self) -> Slot {
        let slot_str = self.payload["message"]["slot"].as_str().unwrap();
        slot_str.parse::<Slot>().unwrap()
    }

    pub fn state_root(&self) -> String {
        self.payload["execution_payload"]["state_root"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fred::types::{RedisMap, RedisValue};
    use serde_json::json;

    #[test]
    fn create_block_submission() {
        let mut submission = BlockSubmission::default();
        let payload =
            json!({"message": {"slot": "42"}, "execution_payload": {"state_root": "some_root"}});
        submission.eligible_at = Some(100);
        submission.payload = payload.clone();
        submission.received_at = 200;
        submission.status_code = Some(400);

        assert_eq!(submission.eligible_at, Some(100));
        assert_eq!(submission.payload, payload);
        assert_eq!(submission.received_at, 200);
        assert_eq!(submission.status_code, Some(400));
    }

    #[test]
    fn test_block_submission_from_redis() {
        let mut redis_map = RedisMap::new();
        redis_map.insert("eligible_at".into(), RedisValue::String("100".into()));
        redis_map.insert(
            "payload".into(),
            RedisValue::String(
                "{\"message\": {\"slot\": \"42\"}, \"execution_payload\": {\"state_root\": \"some_root\"}}"
                    .into(),
            ),
        );
        redis_map.insert("received_at".into(), RedisValue::String("200".into()));
        redis_map.insert("status_code".into(), RedisValue::String("400".into()));
        redis_map.insert(
            "sim_was_simulated".into(),
            RedisValue::String("true".into()),
        );
        redis_map.insert("payload_encoding".into(), RedisValue::String("json".into()));
        redis_map.insert("safe_to_propose".into(), RedisValue::String("true".into()));
        let value: RedisValue = RedisValue::Map(redis_map);
        let result = BlockSubmission::from_value(value);

        dbg!(&result);
        assert!(result.is_ok());
        let submission = result.unwrap();
        assert_eq!(submission.eligible_at, Some(100));
        assert_eq!(submission.received_at, 200);
        assert_eq!(submission.status_code, Some(400));
    }
}
