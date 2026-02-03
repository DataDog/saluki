//! Config stream.
//!
//! This module previously provided `create_config_stream`, which has been moved to
//! `RemoteAgentHelperConfiguration::create_config_stream()` in `agent-data-plane` for
//! better encapsulation and resilience to session ID changes during re-registration.
mod stream;
