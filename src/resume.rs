//! Where to start a video that was played before. The rules are here (no player, no files) so they can be tested.

/// Stopped in the first seconds of a video: not worth coming back to, play it from the start next time.
pub const START_OVER_SECS: f64 = 30.0;
/// Stopped in the last seconds (the credits, the end): the video counts as watched, play it from the start next time.
pub const FINISHED_SECS: f64 = 60.0;

/// The position to remember for a video stopped at `pos` of `duration` seconds (`duration` 0 when not known), or None
/// to forget it so the next play starts at the beginning.
pub fn worth_keeping(pos: f64, duration: f64) -> Option<f64> {
    if !pos.is_finite() || pos < START_OVER_SECS {
        return None;
    }
    if duration > 0.0 && pos > duration - FINISHED_SECS {
        return None;
    }
    Some(pos)
}

/// Where to start a video whose file says it stopped at `saved`: the same rules again, since the file may be old, edited, or
/// from a different cut of the video (a shorter one than the saved position).
pub fn start_for(saved: Option<f64>, duration: f64) -> Option<f64> {
    saved.and_then(|p| worth_keeping(p, duration))
}

#[cfg(test)]
mod resume_tests {
    use super::*;

    #[test]
    fn the_start_and_the_end_of_a_video_are_not_remembered() {
        let hour = 3600.0;
        assert_eq!(worth_keeping(0.0, hour), None);
        assert_eq!(worth_keeping(29.9, hour), None, "still in the first 30 seconds");
        assert_eq!(worth_keeping(30.0, hour), Some(30.0));
        assert_eq!(worth_keeping(1234.5, hour), Some(1234.5));
        assert_eq!(worth_keeping(hour - 60.0, hour), Some(hour - 60.0), "a minute from the end is still worth coming back to");
        assert_eq!(worth_keeping(hour - 59.0, hour), None, "in the last minute the video is done");
        assert_eq!(worth_keeping(hour, hour), None);
    }

    #[test]
    fn a_video_with_no_known_length_and_a_very_short_one() {
        assert_eq!(worth_keeping(600.0, 0.0), Some(600.0), "no duration: nothing to say it is nearly over");
        assert_eq!(worth_keeping(40.0, 80.0), None, "an 80 second clip has no room to resume in");
        assert_eq!(worth_keeping(f64::NAN, 3600.0), None);
        assert_eq!(worth_keeping(f64::INFINITY, 0.0), None);
        assert_eq!(worth_keeping(-5.0, 3600.0), None);
    }

    #[test]
    fn a_saved_position_is_checked_again_when_the_video_opens() {
        assert_eq!(start_for(None, 3600.0), None);
        assert_eq!(start_for(Some(1200.0), 3600.0), Some(1200.0));
        assert_eq!(start_for(Some(1200.0), 900.0), None, "the file is now shorter than where it stopped");
        assert_eq!(start_for(Some(5.0), 3600.0), None);
        assert_eq!(start_for(Some(f64::NAN), 3600.0), None);
    }
}
