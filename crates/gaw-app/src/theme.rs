use egui::Color32;

// Neutral, high-contrast workstation foundation.
pub(crate) const CANVAS: Color32 = Color32::from_gray(18);
pub(crate) const PANEL: Color32 = Color32::from_gray(27);
pub(crate) const PANEL_ALT: Color32 = Color32::from_gray(35);
pub(crate) const PANEL_RAISED: Color32 = Color32::from_gray(43);
pub(crate) const BORDER: Color32 = Color32::from_gray(57);
pub(crate) const BORDER_STRONG: Color32 = Color32::from_gray(92);
pub(crate) const DIM: Color32 = Color32::from_gray(148);
pub(crate) const TEXT: Color32 = Color32::from_gray(228);
pub(crate) const HIGHLIGHT: Color32 = Color32::from_rgb(111, 168, 220);

// Clip hues stay muted and are limited to outlines, waveforms, and small marks.
pub(crate) const AUDIO_TONE: Color32 = Color32::from_rgb(120, 167, 196);
pub(crate) const EVENT_TONE: Color32 = Color32::from_rgb(160, 138, 194);
pub(crate) const NESTED_TONE: Color32 = Color32::from_rgb(116, 181, 165);
pub(crate) const PLAYHEAD: Color32 = Color32::from_rgb(121, 183, 235);
pub(crate) const STATUS_NOTICE: Color32 = Color32::from_rgb(214, 168, 75);
pub(crate) const STATUS_ERROR: Color32 = Color32::from_rgb(224, 90, 90);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workstation_foundation_is_grayscale() {
        for color in [
            CANVAS,
            PANEL,
            PANEL_ALT,
            PANEL_RAISED,
            BORDER,
            BORDER_STRONG,
            DIM,
            TEXT,
        ] {
            assert_eq!(color.r(), color.g());
            assert_eq!(color.g(), color.b());
        }
    }

    #[test]
    fn semantic_accents_are_colored_and_distinct() {
        let accents = [
            HIGHLIGHT,
            AUDIO_TONE,
            EVENT_TONE,
            NESTED_TONE,
            PLAYHEAD,
            STATUS_NOTICE,
            STATUS_ERROR,
        ];
        for color in accents {
            assert!(color.r() != color.g() || color.g() != color.b());
        }
        for (index, color) in accents.iter().enumerate() {
            assert!(!accents[index + 1..].contains(color));
        }
    }
}
