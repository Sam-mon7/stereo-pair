//! RAOP / AirPlay 2 realtime audio data plane: shared clock + timeline, NTP timing
//! server, packet formats, per-speaker sinks.

pub mod packets;
pub mod stream;
pub mod timeline;
pub mod timing;
pub mod timing_server;
