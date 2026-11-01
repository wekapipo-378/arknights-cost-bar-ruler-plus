//! Removed: anonymous usage telemetry.
//!
//! The feature (startup ping + per-session stats upload, gated by the wizard's
//! "发送匿名统计数据" checkbox) was dropped at the user's request; this file is
//! intentionally empty so the module can stay listed in `main.rs` if a future
//! build still references it. Nothing here performs network I/O.
