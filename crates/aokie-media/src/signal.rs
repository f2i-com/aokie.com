use serde::{Deserialize, Serialize};

use crate::MediaError;

pub const MAX_SDP_BYTES: usize = 128 * 1024;
pub const MAX_CANDIDATE_BYTES: usize = 8 * 1024;
pub const MAX_ICE_SERVERS: usize = 8;
pub const MAX_ICE_URLS_PER_SERVER: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SdpSignalType {
    Offer,
    Answer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SdpSignal {
    #[serde(rename = "type")]
    pub kind: SdpSignalType,
    pub sdp: String,
}

impl SdpSignal {
    pub fn validate(&self) -> Result<(), MediaError> {
        if self.sdp.is_empty()
            || self.sdp.len() > MAX_SDP_BYTES
            || self.sdp.contains('\0')
            || !self.sdp.starts_with("v=0")
        {
            return Err(MediaError::InvalidSignal("sdp"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IceCandidateSignal {
    pub sdp_mid: String,
    pub sdp_mline_index: i32,
    pub candidate: String,
}

impl IceCandidateSignal {
    pub fn validate(&self) -> Result<(), MediaError> {
        if self.sdp_mid.len() > 64
            || self.sdp_mid.chars().any(char::is_control)
            || !(0..=64).contains(&self.sdp_mline_index)
            || self.candidate.is_empty()
            || self.candidate.len() > MAX_CANDIDATE_BYTES
            || self.candidate.contains('\0')
        {
            return Err(MediaError::InvalidSignal("ice candidate"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IceServerConfig {
    pub urls: Vec<String>,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub credential: String,
}

impl IceServerConfig {
    pub fn validate_all(servers: &[Self]) -> Result<(), MediaError> {
        if servers.len() > MAX_ICE_SERVERS {
            return Err(MediaError::InvalidSignal("too many ICE servers"));
        }
        for server in servers {
            if server.urls.is_empty()
                || server.urls.len() > MAX_ICE_URLS_PER_SERVER
                || server.username.len() > 512
                || server.credential.len() > 2048
                || server.username.chars().any(char::is_control)
                || server.credential.chars().any(char::is_control)
            {
                return Err(MediaError::InvalidSignal("ICE server"));
            }
            for url in &server.urls {
                let lower = url.to_ascii_lowercase();
                if url.len() > 2048
                    || url.chars().any(char::is_control)
                    || !(lower.starts_with("stun:")
                        || lower.starts_with("stuns:")
                        || lower.starts_with("turn:")
                        || lower.starts_with("turns:"))
                {
                    return Err(MediaError::InvalidSignal("ICE server URL"));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signals_and_ice_configuration_are_bounded() {
        assert!(SdpSignal {
            kind: SdpSignalType::Offer,
            sdp: "v=0\r\n".into()
        }
        .validate()
        .is_ok());
        assert!(SdpSignal {
            kind: SdpSignalType::Offer,
            sdp: "x".repeat(MAX_SDP_BYTES + 1)
        }
        .validate()
        .is_err());
        assert!(IceServerConfig::validate_all(&[IceServerConfig {
            urls: vec!["turns:turn.example:5349?transport=tcp".into()],
            username: "short-lived-user".into(),
            credential: "short-lived-credential".into(),
        }])
        .is_ok());
        assert!(IceServerConfig::validate_all(&[IceServerConfig {
            urls: vec!["https://not-an-ice-server.example".into()],
            username: String::new(),
            credential: String::new(),
        }])
        .is_err());
    }
}
