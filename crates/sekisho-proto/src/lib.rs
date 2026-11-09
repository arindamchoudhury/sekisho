//! gRPC types for the Envoy protocols agentgateway speaks to Sekisho's hooks:
//! the rate-limit service ([`ratelimit`]) and external processing
//! ([`ext_proc`]). Generated at build time from protos vendored from
//! agentgateway v1.7.0.

/// The generated code, nested by protobuf package as it expects.
#[allow(clippy::all, clippy::pedantic, missing_docs)]
pub mod envoy {
    pub mod service {
        pub mod common {
            pub mod v3 {
                tonic::include_proto!("envoy.service.common.v3");
            }
        }
        pub mod ext_proc {
            pub mod v3 {
                tonic::include_proto!("envoy.service.ext_proc.v3");
            }
        }
        pub mod ratelimit {
            pub mod v3 {
                tonic::include_proto!("envoy.service.ratelimit.v3");
            }
        }
    }
}

pub use envoy::service::common::v3 as common;
pub use envoy::service::ext_proc::v3 as ext_proc;
pub use envoy::service::ratelimit::v3 as ratelimit;
