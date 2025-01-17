#![allow(clippy::unwrap_used, reason = "test code panics to indicate errors")]
use super::*;

#[test]
fn filters_chars() {
    let mut atlas_character_map = HashMap::new();
    atlas_character_map.insert('a', 1);
    atlas_character_map.insert('b', 2);

    let filtered_string = FilteredString::new("abcd", &atlas_character_map);
    let filtered_chars: Vec<_> = filtered_string.filtered_chars().collect();

    assert_eq!(filtered_chars, vec!['a', 'b']);
}

#[test]
fn is_empty_when_no_characters_retained() {
    let mut atlas_character_map = HashMap::new();
    atlas_character_map.insert('x', 1);
    atlas_character_map.insert('y', 2);

    let filtered_string = FilteredString::new("abc", &atlas_character_map);

    assert!(filtered_string.is_empty());
}

#[test]
fn is_not_empty_when_characters_retained() {
    let mut atlas_character_map = HashMap::new();
    atlas_character_map.insert('a', 1);

    let filtered_string = FilteredString::new("abc", &atlas_character_map);

    assert!(!filtered_string.is_empty());
}

#[test]
fn display_shows_filtered_text() {
    let mut atlas_character_map = HashMap::new();
    atlas_character_map.insert('a', 1);
    atlas_character_map.insert('b', 2);

    let filtered_string = FilteredString::new("abcd", &atlas_character_map);

    assert_eq!(filtered_string.to_string(), "ab");
}

#[cfg(any(feature = "rendered", feature = "atlas_sprites"))]
#[test]
fn test_image_font_filter_string() {
    use bevy::asset::Handle;
    use bevy_image::ImageSampler;

    use crate::ImageFont;

    let mut atlas_character_map = HashMap::new();
    atlas_character_map.insert('A', 0);
    atlas_character_map.insert('B', 1);
    let font = ImageFont {
        atlas_layout: Handle::default(),
        texture: Handle::default(),
        atlas_character_map: atlas_character_map.clone(),
        image_sampler: ImageSampler::nearest(),
    };

    let input = "ABC";
    let filtered = font.filter_string(input);
    assert_eq!(filtered.to_string(), "AB");
}
