//! Integration tests for dagrun
//!
//! These tests run the actual binary and verify end-to-end behavior.
//! K8s tests require `kind` to be installed and running.

mod basic;
mod k8s;
mod lua_integration;
mod piped_data;
mod services;
