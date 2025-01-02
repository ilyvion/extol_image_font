//! Code for parsing an [`ImageFont`] off of an on-disk representation.

#![expect(clippy::absolute_paths, reason = "false positives")]

use std::io::Error as IoError;
use std::path::PathBuf;

use bevy::{
    asset::{io::Reader, AssetLoader, AsyncReadExt as _, LoadContext, LoadDirectError},
    prelude::*,
    utils::HashMap,
};
use bevy_image::{Image, ImageSampler, ImageSamplerDescriptor};
use camino::{Utf8Path, Utf8PathBuf};
use ron::de::SpannedError;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ImageFont;

/// Human-readable way to specify where the characters in an image font are.
#[derive(Debug, Serialize, Deserialize)]
pub enum ImageFontLayout {
    /// Interprets the string as a "grid" and slices up the input image
    /// accordingly. Leading and trailing newlines are stripped, but spaces
    /// are not (since your font might use them as padding).
    ///
    /// ```rust
    /// # use bevy_image_font::loader::*;
    /// // note that we have a raw string *inside* a raw string here...
    /// let s = r###"
    ///
    /// // this bit is the actual RON syntax
    /// Automatic(r##"
    ///  !"#$%&'()*+,-./0123
    /// 456789:;<=>?@ABCDEFG
    /// HIJKLMNOPQRSTUVWXYZ[
    /// \]^_`abcdefghijklmno
    /// pqrstuvwxyz{|}~
    /// "##)
    ///
    /// "###;
    /// let layout = ron::from_str::<ImageFontLayout>(s).unwrap();
    /// ```
    Automatic(String),

    /// Manually specifies the top-left position of each character, where each
    /// character has the same size. When writing this in RON, the syntax
    /// will look like
    ///
    /// ```rust
    /// # use bevy_image_font::loader::*;
    /// let s = r#"
    /// ManualMonospace(
    ///   size: (4, 8),
    ///   coords: {
    ///      'a': (0, 0),
    ///      'b': (10, 0)
    ///   }
    /// )
    /// "#;
    /// ron::from_str::<ImageFontLayout>(s).unwrap();
    /// ```
    ManualMonospace {
        /// The size of each character, specified as a uniform width and height
        /// in pixels. All characters are assumed to have the same dimensions.
        size: UVec2,

        /// A mapping from characters to their top-left positions within the
        /// font image. Each position is given in pixel coordinates relative
        /// to the top-left corner of the image.
        coords: HashMap<char, UVec2>,
    },

    /// Fully specifies the bounds of each character. The most general case.
    ///
    /// ```rust
    /// # use bevy_image_font::loader::*;
    /// let s = r#"
    /// Manual({
    /// 'a': URect(min: (0, 0), max: (10, 20)),
    /// 'b': URect(min: (20, 20), max: (25, 25))
    /// })
    /// "#;
    /// ron::from_str::<ImageFontLayout>(s).unwrap();
    /// ```
    Manual(HashMap<char, URect>),
}

/// Errors that can show up during validation.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ImageFontLayoutValidationError {
    /// The image width does not evenly divide the character count per line.
    ///
    /// This error occurs when the width of the provided image is not a multiple
    /// of the number of characters per line specified in the layout.
    #[error(
        "Image width {width} is not an exact multiple of per-line character count \
    {per_line_character_count}."
    )]
    InvalidImageWidth {
        /// The width of the image being validated.
        width: u32,
        /// The number of characters per line in the layout.
        per_line_character_count: u32,
    },

    /// The image height does not evenly divide the number of lines.
    ///
    /// This error occurs when the height of the provided image is not a
    /// multiple of the number of lines in the layout.
    #[error("Image height {height} is not an exact multiple of line count {line_count}.")]
    InvalidImageHeight {
        /// The height of the image being validated.
        height: u32,
        /// The number of lines in the layout.
        line_count: u32,
    },

    /// A repeated character was found in an `Automatic` layout string.
    ///
    /// This error occurs when the same character appears multiple times in the
    /// layout string, leading to conflicting placement definitions.
    #[error(
        "The character '{character}' appears more than once. The second appearance is in the \
        layout string at row {row}, column {column}."
    )]
    AutomaticRepeatedCharacter {
        /// The row in the layout string where the repeated character is
        /// located.
        row: usize,
        /// The column in the layout string where the repeated character is
        /// located.
        column: usize,
        /// The character that was repeated in the layout string.
        character: char,
    },
}

impl ImageFontLayout {
    /// Given the image size, returns a map from each codepoint to its location.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "while usize can hold more data than u32, we're working on a number here that \
        should be substantially smaller than even u32's capacity"
    )]
    fn into_char_map(
        self,
        size: UVec2,
    ) -> Result<HashMap<char, URect>, ImageFontLayoutValidationError> {
        match self {
            ImageFontLayout::Automatic(str) => {
                // trim() removes whitespace, which is not what we want!
                let str = str
                    .trim_start_matches(['\r', '\n'])
                    .trim_end_matches(['\r', '\n']);
                #[expect(
                    clippy::expect_used,
                    reason = "this intentionally panics on an empty string. Should never happen as \
                    the ImageFontLayout should always have been validated before this method gets \
                    called"
                )]
                let max_chars_per_line = str
                    .lines()
                    // important: *not* l.len()
                    .map(|line| line.chars().count())
                    .max()
                    .expect("can't create character map from an empty string")
                    as u32;

                if size.x % max_chars_per_line != 0 {
                    return Err(ImageFontLayoutValidationError::InvalidImageWidth {
                        width: size.x,
                        per_line_character_count: max_chars_per_line,
                    });
                }
                let line_count = str.lines().count() as u32;
                if size.y % line_count != 0 {
                    return Err(ImageFontLayoutValidationError::InvalidImageHeight {
                        height: size.y,
                        line_count,
                    });
                }

                let mut rect_map =
                    HashMap::with_capacity((max_chars_per_line * line_count) as usize);

                let rect_width = size.x / max_chars_per_line;
                let rect_height = size.y / line_count;

                for (row, line) in str.lines().enumerate() {
                    for (column, character) in line.chars().enumerate() {
                        let rect = URect::new(
                            rect_width * column as u32,
                            rect_height * row as u32,
                            rect_width * (column + 1) as u32,
                            rect_height * (row + 1) as u32,
                        );
                        if rect_map.insert(character, rect).is_some() {
                            return Err(
                                ImageFontLayoutValidationError::AutomaticRepeatedCharacter {
                                    row,
                                    column,
                                    character,
                                },
                            );
                        }
                    }
                }

                Ok(rect_map)
            }
            ImageFontLayout::ManualMonospace { size, coords } => Ok(coords
                .into_iter()
                .map(|(character, top_left)| {
                    (character, URect::from_corners(top_left, size + top_left))
                })
                .collect()),
            ImageFontLayout::Manual(urect_map) => Ok(urect_map),
        }
    }
}

/// On-disk representation of an [`ImageFont`], optimized to make it easy for
/// humans to write these. See the docs for [`ImageFontLayout`]'s variants for
/// information on how to write the syntax, or [the example font's RON asset].
///
/// [the example font's RON asset](https://github.com/ilyvion/bevy_image_font/blob/main/assets/example_font.image_font.ron)
#[derive(Debug, Serialize, Deserialize)]
pub struct ImageFontDescriptor {
    /// The path to the image file containing the font glyphs, relative to the
    /// RON file. This should be a valid path to a texture file that can be
    /// loaded by the asset system.
    #[deprecated(
        since = "7.1.0",
        note = "This field will become private in the next major version. Use `new` to create a \
        value of this type and `image` to read the field."
    )]
    pub image: Utf8PathBuf,

    /// The layout description of the font, specifying how characters map to
    /// regions within the image. This can use any of the variants provided
    /// by [`ImageFontLayout`], allowing flexible configuration.
    #[deprecated(
        since = "7.1.0",
        note = "This field will become private in the next major version. Use `new` to create a \
        value of this type and `layout` to read the field."
    )]
    pub layout: ImageFontLayout,
}

/// Errors that can show up during validation.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ImageFontDescriptorValidationError {
    /// The image path provided is empty.
    #[error("Image path is empty.")]
    EmptyImagePath,

    /// The layout string used for automatic character placement is empty.
    /// This error occurs when no characters are defined in the automatic layout
    /// string.
    #[error("Automatic layout string is empty.")]
    EmptyLayoutString,
}

#[expect(
    missing_docs,
    reason = "temporary type alias for a deprecated, renamed type"
)]
#[deprecated(
    since = "7.1.0",
    note = "ImageFontSettings was renamed to ImageFontDescriptor and will be removed in version 8.0"
)]
pub type ImageFontSettings = ImageFontDescriptor;

impl ImageFontDescriptor {
    /// Creates a new `ImageFontDescriptor` instance with the provided image
    /// path and font layout, performing validation to ensure the descriptor
    /// is valid.
    ///
    /// # Parameters
    /// - `image`: The path to the image file containing the font glyphs,
    ///   relative to the RON file that described the font. This should be a
    ///   valid path to a texture file that can be loaded by the asset system.
    /// - `layout`: The layout description of the font, specifying how
    ///   characters map to regions within the image. See [`ImageFontLayout`]
    ///   for more details about the available layout configurations.
    ///
    /// # Returns
    /// A new `ImageFontDescriptor` instance if validation succeeds.
    ///
    /// # Errors
    /// Returns an [`ImageFontDescriptorValidationError`] if the provided values
    /// do not pass validation.
    #[expect(deprecated, reason = "fields are only deprecated externally")]
    pub fn new(
        image: Utf8PathBuf,
        layout: ImageFontLayout,
    ) -> Result<Self, ImageFontDescriptorValidationError> {
        let value = Self { image, layout };
        value.validate()?;
        Ok(value)
    }

    /// Validates the `ImageFontDescriptor` struct to ensure all required fields
    /// are populated.
    ///
    /// # Errors
    ///   - `ImageFontLoadError::EmptyImagePath` if the `image` path is empty.
    ///   - `ImageFontLoadError::EmptyLayoutString` if the `layout` string for
    ///     `Automatic` is empty.
    ///
    /// # Example
    /// ```rust
    /// # use camino::Utf8PathBuf;
    /// # use bevy_image_font::loader::{ImageFontLayout, ImageFontDescriptor};
    ///
    /// let settings = ImageFontDescriptor {
    ///     image: Utf8PathBuf::from("path/to/font.png"),
    ///     layout: ImageFontLayout::Automatic("ABCDEF".into()),
    /// };
    /// assert!(settings.validate().is_ok());
    /// ```
    #[deprecated(
        since = "7.1.0",
        note = "This method will become private in the next major version. When using `new` to create a \
        value of this type, `validate` gets invoked automatically."
    )]
    #[expect(deprecated, reason = "fields are only deprecated externally")]
    pub fn validate(&self) -> Result<(), ImageFontDescriptorValidationError> {
        if self.image.as_str().trim().is_empty() {
            return Err(ImageFontDescriptorValidationError::EmptyImagePath);
        }
        if matches!(self.layout, ImageFontLayout::Automatic(ref layout) if layout.trim().is_empty())
        {
            return Err(ImageFontDescriptorValidationError::EmptyLayoutString);
        }
        Ok(())
    }

    /// Gets the path to the image file containing the font glyphs.
    ///
    /// This is the value of the `image` field. The path is relative to the
    /// RON file and should point to a valid texture file.
    ///
    /// # Returns
    /// A reference to the `Utf8PathBuf` containing the image file path.
    #[must_use]
    pub fn image(&self) -> &Utf8Path {
        #[expect(deprecated, reason = "field is only deprecated externally")]
        &self.image
    }

    /// Gets the layout description of the font.
    ///
    /// This is the value of the `layout` field, which specifies how characters
    /// map to regions within the image. See [`ImageFontLayout`] for details
    /// about the available variants.
    ///
    /// # Returns
    /// A reference to the `ImageFontLayout` describing the font layout.
    #[must_use]
    pub fn layout(&self) -> &ImageFontLayout {
        #[expect(deprecated, reason = "field is only deprecated externally")]
        &self.layout
    }
}

/// Loader for [`ImageFont`]s.
#[derive(Debug, Default)]
pub struct ImageFontLoader;

/// Errors that can show up during loading.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ImageFontLoadError {
    /// Parsing the on-disk representation of the font failed. This typically
    /// indicates a syntax or formatting error in the RON file.
    #[error("couldn't parse on-disk representation: {0}")]
    ParseFailure(#[from] SpannedError),

    /// The image path provided in the settings is empty. This error occurs
    /// when no valid file path is specified for the font image.
    #[error("Image path is empty.")]
    #[deprecated(
        since = "7.1.0",
        note = "No longer in use and will be removed in version 8.0. Use `ValidationError` instead."
    )]
    EmptyImagePath,

    /// The layout string used for automatic character placement is empty.
    /// This error occurs when no characters are defined in the layout string.
    #[error("Automatic layout string is empty.")]
    #[deprecated(
        since = "7.1.0",
        note = "No longer in use and will be removed in version 8.0. Use `ValidationError` instead."
    )]
    EmptyLayoutString,

    /// A validation error occurred on the `ImageFontDescriptor`. Inspect the
    /// value of the inner error for details.
    #[error("Font descriptor is invalid: {0}")]
    DescriptorValidationError(#[from] ImageFontDescriptorValidationError),

    /// A validation error occurred on the `ImageFontDescriptor`. Inspect the
    /// value of the inner error for details.
    #[error("Font layout is invalid: {0}")]
    LayoutValidationError(#[from] ImageFontLayoutValidationError),

    /// An I/O error occurred while loading the image font. This might happen
    /// if the file cannot be accessed, is missing, or is corrupted.
    #[error("i/o error when loading image font: {0}")]
    Io(#[from] IoError),

    /// Failed to load an asset directly. This is usually caused by an error
    /// in the asset pipeline or a missing dependency.
    #[error("failed to load asset: {0}")]
    LoadDirect(Box<LoadDirectError>),

    /// The path provided for the font's image was not loaded as an image. This
    /// may occur if the file is in an unsupported format or if the path is
    /// incorrect.
    #[error("Path does not point to a valid image file: {0}")]
    NotAnImage(Utf8PathBuf),

    /// The path provided for the font's image was not loaded as an image. This
    /// may occur if the file is in an unsupported format or if the path is
    /// incorrect.
    #[error("Path is not valid UTF-8: {0:?}")]
    InvalidPath(PathBuf),

    /// The asset path has no parent directory.
    #[error("Asset path has no parent directory")]
    MissingParentPath,
}

impl From<LoadDirectError> for ImageFontLoadError {
    #[inline]
    fn from(value: LoadDirectError) -> Self {
        Self::LoadDirect(Box::new(value))
    }
}

/// Configuration settings for the `ImageFontLoader`.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct ImageFontLoaderSettings {
    /// The [`ImageSampler`] to use during font image rendering. Determines
    /// how the font's texture is sampled when scaling or transforming it.
    ///
    /// The default is `nearest`, which scales the image without blurring,
    /// preserving a crisp, pixelated appearance. This is usually ideal for
    /// pixel-art fonts.
    pub image_sampler: ImageSampler,
}

impl Default for ImageFontLoaderSettings {
    fn default() -> Self {
        Self {
            image_sampler: ImageSampler::Descriptor(ImageSamplerDescriptor::nearest()),
        }
    }
}

impl AssetLoader for ImageFontLoader {
    type Asset = ImageFont;

    type Settings = ImageFontLoaderSettings;

    type Error = ImageFontLoadError;

    async fn load(
        &self,
        reader: &mut dyn Reader,
        settings: &Self::Settings,
        load_context: &mut LoadContext<'_>,
    ) -> Result<Self::Asset, Self::Error> {
        let mut str = String::new();
        reader.read_to_string(&mut str).await?;

        let disk_format: ImageFontDescriptor = ron::from_str(&str)?;

        #[expect(deprecated, reason = "method is only deprecated externally")]
        disk_format.validate()?;

        // need the image loaded immediately because we need its size
        let image_path = load_context
            .path()
            .parent()
            .ok_or(ImageFontLoadError::MissingParentPath)?
            .join(disk_format.image());
        let Some(mut image) = load_context
            .loader()
            .immediate()
            .with_unknown_type()
            .load(image_path.as_path())
            .await?
            .take::<Image>()
        else {
            let path = match Utf8PathBuf::from_path_buf(image_path) {
                Ok(path) => path,
                Err(image_path) => return Err(ImageFontLoadError::InvalidPath(image_path)),
            };
            return Err(ImageFontLoadError::NotAnImage(path));
        };

        image.sampler = settings.image_sampler.clone();

        let size = image.size();
        #[expect(deprecated, reason = "fields are only deprecated externally")]
        let char_map = disk_format.layout.into_char_map(size)?;
        let image_handle = load_context.add_labeled_asset(String::from("texture"), image);

        let (map, layout) = ImageFont::mapped_atlas_layout_from_char_map(size, &char_map);
        let layout_handle = load_context.add_labeled_asset(String::from("layout"), layout);

        let image_font = ImageFont::from_mapped_atlas_layout(
            image_handle,
            map,
            layout_handle,
            settings.image_sampler.clone(),
        );
        Ok(image_font)
    }

    fn extensions(&self) -> &[&str] {
        &["image_font.ron"]
    }
}

#[cfg(test)]
mod tests;
