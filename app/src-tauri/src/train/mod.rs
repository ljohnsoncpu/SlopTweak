//! LoRA training on a rented GPU: settings, dataset audit and the run
//! lifecycle, the instance client and the job upload. Tested against
//! `MockProvider` and a fake HTTP server; the UI comes in Phase T3 (PLAN.md).
#![allow(dead_code)] // The run manager that uses these arrives with the UI (T3).

pub mod client;
pub mod commands;
pub mod dataset;
pub mod diag;
pub mod job;
pub mod local;
pub mod manager;
pub mod moss;
pub mod recent;
pub mod run;
pub mod settings;
pub mod sim;
pub mod trainer_setup;
