#[macro_use]
extern crate rocket;

use blake3::Hasher;
use fast_image_resize::{FilterType, ResizeAlg, ResizeOptions, Resizer};
use image::{DynamicImage, ExtendedColorType, ImageEncoder, codecs::avif::AvifEncoder};
use reqwest::Client;
use rocket::{
    data::{Data, ToByteUnit},
    http::Status,
    request::Request,
    response::{Responder, Response},
    serde::json::{Value as SerdeJsonValue, json},
};
use serde::Deserialize;
use std::{fmt, fs, fs::File, path::Path};

#[derive(Debug, PartialEq)]
pub struct ResizedDimensions {
    pub width: u32,
    pub height: u32,
}

pub trait ImageDimensions {
    fn width(&self) -> u32;
    fn height(&self) -> u32;
}

impl ImageDimensions for DynamicImage {
    fn width(&self) -> u32 {
        self.width()
    }
    fn height(&self) -> u32 {
        self.height()
    }
}

pub fn calculate_resized_dimensions<T: ImageDimensions>(
    image: &T,
    tallest_side: u32,
) -> ResizedDimensions {
    let (original_width, original_height) = (image.width(), image.height());

    if original_width == 0 || original_height == 0 || tallest_side == 0 {
        return ResizedDimensions {
            width: 0,
            height: 0,
        };
    }

    let aspect_ratio = original_width as f64 / original_height as f64;

    let (new_width, new_height) = if original_width >= original_height {
        (
            tallest_side,
            (tallest_side as f64 / aspect_ratio).round() as u32,
        )
    } else {
        (
            (tallest_side as f64 * aspect_ratio).round() as u32,
            tallest_side,
        )
    };

    ResizedDimensions {
        width: new_width,
        height: new_height,
    }
}

#[derive(Deserialize)]
struct ResizeRequest {
    tallestSide: u32,
    source: String,
    format: Option<String>,
}

fn calculate_hash(input: &str) -> String {
    let mut hasher = Hasher::new();
    hasher.update(input.as_bytes());
    hasher.finalize().to_hex().to_string()
}

#[post("/", format = "json", data = "<data>")]
async fn resize_handler(data: Data<'_>) -> Result<SerdeJsonValue, CustomError> {
    let request: ResizeRequest = serde_json::from_str(
        &String::from_utf8(
            data.open(1024.bytes())
                .into_bytes()
                .await
                .map_err(|_| CustomError::BadRequest)?
                .into_inner(),
        )
        .map_err(|_| CustomError::BadRequest)?,
    )?;

    if request.format.as_deref().unwrap_or("avif") != "avif" {
        return Err(CustomError::UnsupportedFormat);
    }

    let hash_str = calculate_hash(&request.source);
    let resized_filename = format!("{hash_str}_{}.avif", request.tallestSide);
    let images_dir = Path::new("images");
    fs::create_dir_all(images_dir).map_err(|_| CustomError::DirectoryCreationError)?;
    let resized_image_path = images_dir.join(&resized_filename);

    if resized_image_path.exists() {
        return Ok(json!({
            "status": "ALREADY_TRANSFORMED",
            "hash": hash_str,
            "filename": resized_filename,
        }));
    }

    let downloaded_image_path = images_dir.join(&hash_str);
    let image_bytes: Vec<u8> = match fs::read(&downloaded_image_path) {
        Ok(bytes) => {
            if bytes.is_empty() {
                // Check for empty file (corrupt)
                return Err(CustomError::FileCorruptError); // Treat empty file as read error
            }
            bytes
        }
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                // File not found, proceed to download
                Client::new()
                    .get(&request.source)
                    .send()
                    .await
                    .map_err(|_| CustomError::DownloadError)?
                    .bytes()
                    .await
                    .map_err(|_| CustomError::DownloadError)?
                    .to_vec()
            } else {
                // Other file read errors (permission, etc.)
                return Err(CustomError::FileReadError);
            }
        }
    };

    if image_bytes.is_empty() {
        // Check for empty download as well
        return Err(CustomError::BadRequest);
    }

    let img = image::load_from_memory(&image_bytes).map_err(|_| CustomError::ImageDecodeError)?;
    let resized_dims = calculate_resized_dimensions(&img, request.tallestSide);
    let mut dst_image = DynamicImage::new(resized_dims.width, resized_dims.height, img.color());

    Resizer::new()
        .resize(
            &img,
            &mut dst_image,
            &ResizeOptions::new()
                .resize_alg(ResizeAlg::Convolution(FilterType::CatmullRom))
                .use_alpha(false),
        )
        .map_err(|_| CustomError::ResizeError)?;

    fs::write(&downloaded_image_path, &image_bytes).map_err(|_| CustomError::FileWriteError)?;

    let output_file =
        File::create(&resized_image_path).map_err(|_| CustomError::FileCreationError)?;
    AvifEncoder::new_with_speed_quality(output_file, 6, 85)
        .write_image(
            dst_image.as_bytes(),
            resized_dims.width,
            resized_dims.height,
            ExtendedColorType::from(img.color()),
        )
        .map_err(|_| CustomError::ImageEncodeError)?;

    Ok(json!({
        "status": "TRANSFORMED",
        "hash": hash_str,
        "filename": resized_filename,
    }))
}

#[derive(Debug)]
enum CustomError {
    UnsupportedFormat,
    DownloadError,
    ImageDecodeError,
    ResizeError,
    FileCreationError,
    ImageEncodeError,
    DirectoryCreationError,
    FileWriteError,
    FileReadError,
    FileCorruptError,
    BadRequest,
    JsonDeserializeError(String), // Add specific error for JSON deserialization
}

impl fmt::Display for CustomError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            CustomError::UnsupportedFormat => {
                write!(
                    f,
                    "Unsupported image format requested. Only 'avif' is supported."
                )
            }
            CustomError::DownloadError => write!(f, "Failed to download image from URL."),
            CustomError::ImageDecodeError => write!(
                f,
                "Failed to decode image data. Image might be corrupted or unsupported format."
            ),
            CustomError::ResizeError => write!(f, "Failed to resize image."),
            CustomError::FileCreationError => write!(f, "Failed to create output file on disk."),
            CustomError::ImageEncodeError => write!(f, "Failed to encode image to AVIF format."),
            CustomError::DirectoryCreationError => {
                write!(f, "Failed to create necessary directories on disk.")
            }
            CustomError::FileWriteError => write!(f, "Failed to write image data to file on disk."),
            CustomError::FileReadError => write!(f, "Failed to read image data from file on disk."),
            CustomError::FileCorruptError => {
                write!(f, "File was read but was empty, likely corrupt.")
            }
            CustomError::BadRequest => write!(f, "Bad request: invalid request format."),
            CustomError::JsonDeserializeError(details) => {
                write!(f, "Bad request: invalid JSON - {}", details)
            }
        }
    }
}

impl From<CustomError> for SerdeJsonValue {
    fn from(error: CustomError) -> Self {
        json!({ "status": "ERROR", "reason": error.to_string() })
    }
}

impl<'r> Responder<'r, 'static> for CustomError {
    fn respond_to(self, request: &'r Request) -> Result<Response<'static>, Status> {
        SerdeJsonValue::from(self).respond_to(request)
    }
}

impl From<serde_json::Error> for CustomError {
    fn from(error: serde_json::Error) -> Self {
        CustomError::JsonDeserializeError(error.to_string())
    }
}

#[launch]
fn rocket() -> _ {
    rocket::build().mount("/", routes![resize_handler])
}
