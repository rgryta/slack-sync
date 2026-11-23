//! Models, the connection pool and every SQL statement in the project.
//!
//! The split mirrors the Python package it was ported from: `store` owns all SQL and
//! nothing outside it writes a query.

pub mod config;
pub mod db;
pub mod models;
pub mod store;
