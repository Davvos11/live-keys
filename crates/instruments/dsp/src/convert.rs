//! Unit conversions.

/// Decibels to a linear gain factor.
#[inline]
pub fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// A linear gain factor to decibels (−∞ for 0).
#[inline]
pub fn gain_to_db(gain: f32) -> f32 {
    20.0 * gain.log10()
}

/// Cents to a frequency ratio.
#[inline]
pub fn cents_to_ratio(cents: f32) -> f32 {
    (cents / 1200.0).exp2()
}

/// A MIDI note number (69 = A4 = 440 Hz) to a frequency in Hz.
#[inline]
pub fn note_to_hz(note: f32) -> f32 {
    440.0 * ((note - 69.0) / 12.0).exp2()
}
