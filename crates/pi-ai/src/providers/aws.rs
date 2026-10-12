//! AWS request plumbing shared by the Bedrock provider: SigV4 signing, event stream
//! framing, partition endpoints, and the credential chain.

pub mod credentials;
pub mod endpoint;
pub mod event_stream;
mod profile;
mod remote;
pub mod sigv4;
