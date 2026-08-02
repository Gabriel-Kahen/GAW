use egui::Color32;

// Neutral, high-contrast workstation palette. Keep every RGB triplet achromatic.
pub(crate) const CANVAS: Color32 = Color32::from_gray(18);
pub(crate) const PANEL: Color32 = Color32::from_gray(27);
pub(crate) const PANEL_ALT: Color32 = Color32::from_gray(35);
pub(crate) const PANEL_RAISED: Color32 = Color32::from_gray(43);
pub(crate) const BORDER: Color32 = Color32::from_gray(57);
pub(crate) const BORDER_STRONG: Color32 = Color32::from_gray(92);
pub(crate) const DIM: Color32 = Color32::from_gray(148);
pub(crate) const TEXT: Color32 = Color32::from_gray(228);
pub(crate) const HIGHLIGHT: Color32 = Color32::from_gray(210);

// Clip classes use luminance, labels, and internal marks instead of hue.
pub(crate) const AUDIO_TONE: Color32 = Color32::from_gray(158);
pub(crate) const EVENT_TONE: Color32 = Color32::from_gray(124);
pub(crate) const NESTED_TONE: Color32 = Color32::from_gray(184);
pub(crate) const PLAYHEAD: Color32 = Color32::from_gray(238);
pub(crate) const STATUS_NOTICE: Color32 = Color32::from_gray(188);
pub(crate) const STATUS_ERROR: Color32 = Color32::from_gray(236);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_is_strictly_grayscale() {
        for color in [
            CANVAS,
            PANEL,
            PANEL_ALT,
            PANEL_RAISED,
            BORDER,
            BORDER_STRONG,
            DIM,
            TEXT,
            HIGHLIGHT,
            AUDIO_TONE,
            EVENT_TONE,
            NESTED_TONE,
            PLAYHEAD,
            STATUS_NOTICE,
            STATUS_ERROR,
        ] {
            assert_eq!(color.r(), color.g());
            assert_eq!(color.g(), color.b());
        }
    }
}
