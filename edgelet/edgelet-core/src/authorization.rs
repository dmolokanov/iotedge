// Copyright (c) Microsoft. All rights reserved.

use std::fmt;

#[derive(Debug)]
pub enum Policy {
    Anonymous,
    Caller,
    Module(&'static str),
}

#[derive(Clone, Debug, PartialEq)]
pub enum AuthId {
    None,
    Any,
    Value(String),
}

impl fmt::Display for AuthId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthId::None => write!(f, "none"),
            AuthId::Any => write!(f, "any"),
            AuthId::Value(auth_id) => write!(f, "{}", auth_id),
        }
    }
}

pub struct Authorization {
    policy: Policy,
}

impl Authorization {
    pub fn new(policy: Policy) -> Self {
        Authorization { policy }
    }

    pub fn authorize(&self, name: Option<String>, auth_id: AuthId) -> bool {
        let name = name.map(|n| n.trim_start_matches('$').to_string());
        match self.policy {
            Policy::Anonymous => self.auth_anonymous(),
            Policy::Caller => self.auth_caller(name, auth_id),
            Policy::Module(ref expected_name) => self.auth_module(expected_name, auth_id),
        }
    }

    fn auth_anonymous(&self) -> bool {
        true
    }

    fn auth_caller(&self, name: Option<String>, auth_id: AuthId) -> bool {
        name.map_or_else(
            || false,
            |name| match auth_id {
                AuthId::None => false,
                AuthId::Any => true,
                AuthId::Value(module) => module == name,
            },
        )
    }

    fn auth_module(&self, expected_name: &'static str, auth_id: AuthId) -> bool {
        self.auth_caller(Some(expected_name.to_string()), auth_id)
    }
}

#[cfg(test)]
mod tests {
    use crate::{AuthId, Authorization, Policy};

    #[test]
    fn should_authorize_anonymous() {
        let auth = Authorization::new(Policy::Anonymous);
        assert_eq!(true, auth.authorize(None, AuthId::None));
    }

    #[test]
    fn should_authorize_caller() {
        let auth = Authorization::new(Policy::Caller);
        assert_eq!(
            true,
            auth.authorize(Some("abc".to_string()), AuthId::Value("abc".to_string()))
        );
    }

    #[test]
    fn should_authorize_system_caller() {
        let auth = Authorization::new(Policy::Caller);
        assert_eq!(
            true,
            auth.authorize(
                Some("$edgeAgent".to_string()),
                AuthId::Value("edgeAgent".to_string()),
            )
        );
    }

    #[test]
    fn should_reject_caller_without_name() {
        let auth = Authorization::new(Policy::Caller);
        assert_eq!(
            false,
            auth.authorize(None, AuthId::Value("abc".to_string()))
        );
    }

    #[test]
    fn should_reject_caller_with_different_name() {
        let auth = Authorization::new(Policy::Caller);
        assert_eq!(
            false,
            auth.authorize(Some("xyz".to_string()), AuthId::Value("abc".to_string()))
        );
    }

    #[test]
    fn should_authorize_module() {
        let auth = Authorization::new(Policy::Module("abc"));
        assert_eq!(true, auth.authorize(None, AuthId::Value("abc".to_string())));
    }

    #[test]
    fn should_reject_module_whose_name_does_not_match_policy() {
        let auth = Authorization::new(Policy::Module("abc"));
        assert_eq!(
            false,
            auth.authorize(None, AuthId::Value("xyz".to_string()))
        );
    }
}
