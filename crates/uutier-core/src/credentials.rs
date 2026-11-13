//! UU 账号凭据。

use std::fmt;

use serde::{Deserialize, Serialize};

/// UU PC 客户端内置的请求签名密钥。
pub const DEFAULT_UU_SIGN_KEY: &str = "alWiSzXZTLu3WfFnw13uBru3";

/// UU 账号凭据。字段名与 uu-remote 的 `config.json` 一致。
#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct UuCredentials {
    pub user_id: String,
    pub device_id: String,
    pub client_id: String,
    pub system_id: String,
    /// uu-remote 首次注册设备时生成，登录后保持不变。
    pub machine_guid: String,
    pub token: String,
    pub csrf: String,
    /// 为空时使用 [`DEFAULT_UU_SIGN_KEY`]。
    pub sign_key: String,
}

impl fmt::Debug for UuCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("UuCredentials { .. }")
    }
}

impl UuCredentials {
    /// 返回缺失的必填字段。
    pub fn missing_fields(&self) -> Vec<&'static str> {
        [
            ("user_id", &self.user_id),
            ("device_id", &self.device_id),
            ("client_id", &self.client_id),
            ("system_id", &self.system_id),
            ("token", &self.token),
            ("csrf", &self.csrf),
        ]
        .into_iter()
        .filter(|(_, value)| value.is_empty())
        .map(|(name, _)| name)
        .collect()
    }

    /// 返回实际使用的签名密钥。
    pub fn effective_sign_key(&self) -> &str {
        if self.sign_key.is_empty() {
            DEFAULT_UU_SIGN_KEY
        } else {
            &self.sign_key
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_does_not_expose_secrets() {
        let credentials = UuCredentials {
            user_id: "user".to_owned(),
            device_id: "device".to_owned(),
            client_id: "client".to_owned(),
            system_id: "system".to_owned(),
            machine_guid: "machine".to_owned(),
            token: "secret-token".to_owned(),
            csrf: "secret-csrf".to_owned(),
            sign_key: "secret-sign-key".to_owned(),
        };
        let text = format!("{credentials:?}");
        assert_eq!(text, "UuCredentials { .. }");
    }

    #[test]
    fn reports_missing_fields() {
        let credentials = UuCredentials {
            user_id: "user".to_owned(),
            token: "token".to_owned(),
            ..UuCredentials::default()
        };
        let missing = credentials.missing_fields();
        assert!(missing.contains(&"device_id"));
        assert!(missing.contains(&"client_id"));
        assert!(missing.contains(&"system_id"));
        assert!(missing.contains(&"csrf"));
        assert!(!missing.contains(&"user_id"));
        assert!(!missing.contains(&"token"));
    }

    #[test]
    fn uses_default_sign_key() {
        let credentials = UuCredentials::default();
        assert_eq!(credentials.effective_sign_key(), DEFAULT_UU_SIGN_KEY);
    }

    #[test]
    fn deserializes_partial_credentials() {
        let credentials: UuCredentials =
            serde_json::from_str(r#"{"user_id":"u","token":"secret-token"}"#).unwrap();
        assert_eq!(credentials.user_id, "u");
        assert_eq!(credentials.token, "secret-token");
        assert!(credentials.machine_guid.is_empty());
    }
}
