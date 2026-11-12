//! The harness core: what loads, plans and runs a case, independent of what the case's participants run.
//!
//! A case lists participants, connects them, and runs a procedure of steps. A participant is data, a
//! [`spec::ParticipantSpec`], and a participant type is a function that fills it in for one piece of software. The
//! case's own fields are laid over that, so a participant with no participant type is one the case writes in full.
//! Planning builds every participant's spec for each OS a case declares and checks it, so a mistake for any OS is found
//! on any host, before anything starts.
//!
//! Core names neither participant types nor steps: the binary registers both in a [`registry::Registry`].

pub(crate) mod backend;
pub(crate) mod case;
// TODO: remove the dead_code allowance once participant cases run.
#[allow(dead_code)]
pub(crate) mod container;
// TODO: remove the dead_code allowance once participant cases run.
#[allow(dead_code)]
pub(crate) mod courier;
pub(crate) mod deadline;
pub(crate) mod ledger;
mod order;
pub(crate) mod participant;
pub(crate) mod plan;
mod print;
pub(crate) mod probe;
pub(crate) mod record;
pub(crate) mod reference;
pub(crate) mod registry;
pub(crate) mod result;
pub(crate) mod spec;
pub(crate) mod step;
pub(crate) mod tasks;
pub(crate) mod template;
pub(crate) mod wiring;

#[cfg(test)]
mod daemon_tests;
