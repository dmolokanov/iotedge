// Copyright (c) Microsoft. All rights reserved.

use super::Policy;
use crate::error::Error;
use futures::{future, Future};
use crate::module::ModuleRuntime;
use log::info;
use std::{fmt, cmp};
use futures::future::Either;
use crate::ModuleRuntimeErrorReason;

pub struct Authorization<M>
    where
        M: 'static + ModuleRuntime,
{
    runtime: M,
    policy: Policy,
}

impl<M> Authorization<M>
    where
        M: 'static + ModuleRuntime,
        for<'r> &'r <M as ModuleRuntime>::Error: Into<ModuleRuntimeErrorReason>,
{
    pub fn new(runtime: M, policy: Policy) -> Self {
        Authorization { runtime, policy }
    }

    pub fn authorize(
        &self,
        name: Option<String>,
        auth_id: AuthId,
    ) -> impl Future<Item=bool, Error=Error> {
        let name = name.map(|n| n.trim_start_matches('$').to_string());
        match self.policy {
            Policy::Anonymous => Either::A(Either::A(self.auth_anonymous())),
            Policy::Caller => Either::A(Either::B(self.auth_caller(name, auth_id))),
            Policy::Module(ref expected_name) => Either::B(self.auth_module(expected_name, auth_id)),
        }
    }

    fn auth_anonymous(&self) -> impl Future<Item=bool, Error=Error> {
        future::ok(true)
    }

    fn auth_caller(
        &self,
        name: Option<String>,
        auth_id: AuthId,
    ) -> impl Future<Item=bool, Error=Error> {
        info!("authorize caller: name = {:?} auth_id = {:?}", name, auth_id);
        future::ok(false)
    }

    fn auth_module(
        &self,
        expected_name: &'static str,
        auth_id: AuthId,
    ) -> impl Future<Item=bool, Error=Error> {
        let _x = &self.runtime;
        info!("authorize module: name = {:?} auth_id = {:?}", expected_name, auth_id);
        future::ok(false)
    }
}

// todo make runtime specific
#[derive(Clone, Debug)]
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
            AuthId::Value(pid) => write!(f, "{}", pid),
        }
    }
}

/// AuthId are considered not equal when compared against
/// None, or equal when compared against Any. None takes
/// precedence, so Any is not equal to None.
impl cmp::PartialEq for AuthId {
    fn eq(&self, other: &AuthId) -> bool {
        match self {
            AuthId::None => false,
            AuthId::Any => match *other {
                AuthId::None => false,
                _ => true,
            },
            AuthId::Value(token1) => match other {
                AuthId::None => false,
                AuthId::Any => true,
                AuthId::Value(token2) => token1 == token2,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_eq() {
        assert_ne!(AuthId::None, AuthId::None);
        assert_ne!(AuthId::None, AuthId::Any);
        assert_ne!(AuthId::None, AuthId::Value("token".to_string()));
        assert_ne!(AuthId::Any, AuthId::None);
        assert_eq!(AuthId::Any, AuthId::Any);
        assert_eq!(AuthId::Any, AuthId::Value("token".to_string()));
        assert_ne!(AuthId::Value("token".to_string()), AuthId::None);
        assert_eq!(AuthId::Value("token".to_string()), AuthId::Any);
        assert_eq!(AuthId::Value("token".to_string()), AuthId::Value("token".to_string()));
        assert_ne!(AuthId::Value("token1".to_string()), AuthId::Value("token2".to_string()));
    }
}
