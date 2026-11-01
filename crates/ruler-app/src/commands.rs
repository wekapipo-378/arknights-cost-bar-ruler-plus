use crate::ui_state::FrameDisplayMode;

#[derive(Clone, Debug, PartialEq)]
pub enum UiCommand {
    PrepareCalibration,
    StartCalibration,
    UseProfile {
        filename: String,
    },
    #[allow(dead_code)]
    RenameProfile {
        old: String,
        new_base: String,
    },
    DeleteProfile {
        filename: String,
    },
    SetDisplayMode(FrameDisplayMode),
    AdjustTimer {
        frames: i32,
    },
    SetTimer {
        frames: i32,
    },
    ResetTimer,
    UndoResetTimer,
    ToggleLapTimer,
    RecordFrameMark {
        frame: u64,
    },
    TogglePauseMode {
        enabled: bool,
    },
    SelectAxis {
        name: String,
    },
    SetPauseLead {
        frames: i32,
    },
    /// Persist the overlay scale multiplier (1.0 == 100%).
    SetOverlayScale(f32),
    /// Persist the global hotkey bindings (empty string == unbound).
    SetHotkeys {
        record_frame: String,
        pause_mode: String,
        lap: String,
    },
    /// Persist the overlay window position (screen pixels).
    SaveOverlayPlacement {
        x: i32,
        y: i32,
    },
    Exit,
}
