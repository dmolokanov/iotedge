// Copyright (c) Microsoft. All rights reserved.

use super::Policy;
use crate::error::Error;
use futures::{future, Future};
use crate::module::ModuleRuntime;
use crate::pid::Pid;
use log::info;

pub struct Authorization<M>
    where
        M: 'static + ModuleRuntime,
{
    _runtime: M,
    _policy: Policy,
}

impl<M> Authorization<M>
    where
        M: 'static + ModuleRuntime,
{
    pub fn new(_runtime: M, _policy: Policy) -> Self {
        Authorization { _runtime, _policy }
    }

    pub fn authorize(
        &self,
        name: Option<String>,
        pid: Pid,
    ) -> impl Future<Item = bool, Error = Error> {
        info!("authorize: name = {:?} auth_id {:?}", name, pid);
        future::ok(true)
    }
}
