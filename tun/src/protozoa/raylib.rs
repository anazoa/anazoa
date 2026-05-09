// use std::ffi::{CString, c_char};
use std::ffi::c_void;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

// #[repr(C)]
// #[derive(Clone, Copy)]
// pub struct Vector2 {
//     pub x: f32,
//     pub y: f32,
// }

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Rectangle {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Image {
    pub data: *mut c_void,
    pub width: i32,
    pub height: i32,
    pub mipmaps: i32,
    pub format: i32,
}

unsafe extern "C" {
    fn GenImageColor(width: i32, height: i32, color: Color) -> Image;
    fn UnloadImage(image: Image);
    // fn ExportImage(image: Image, file_name: *const c_char) -> bool;
    fn LoadImageColors(image: Image) -> *mut Color;
    fn UnloadImageColors(colors: *mut Color);

    // fn ImageDrawLine(dst: *mut Image, x1: i32, y1: i32, x2: i32, y2: i32, color: Color);
    // fn ImageDrawLineEx(dst: *mut Image, start: Vector2, end: Vector2, thick: i32, color: Color);
    fn ImageDrawCircle(dst: *mut Image, center_x: i32, center_y: i32, radius: i32, color: Color);
    fn ImageDrawCircleLines(
        dst: *mut Image,
        center_x: i32,
        center_y: i32,
        radius: i32,
        color: Color,
    );
    fn ImageDrawRectangle(dst: *mut Image, x: i32, y: i32, width: i32, height: i32, color: Color);
    fn ImageDrawRectangleLines(dst: *mut Image, rec: Rectangle, thick: i32, color: Color);
    // fn ImageDrawTriangle(dst: *mut Image, v1: Vector2, v2: Vector2, v3: Vector2, color: Color);
}

pub struct Canvas {
    image: Image,
}

impl Canvas {
    pub fn new(width: i32, height: i32, background: Color) -> Self {
        let image = unsafe { GenImageColor(width, height, background) };
        Self { image }
    }

    // pub fn export(&self, path: &str) {
    //     let path = CString::new(path).expect("path contains interior null byte");
    //     let ok = unsafe { ExportImage(self.image, path.as_ptr()) };
    //     assert!(ok, "failed to export image");
    // }

    pub fn rgba_pixels(&self) -> Vec<Color> {
        let colors = unsafe { LoadImageColors(self.image) };
        assert!(!colors.is_null(), "failed to read image pixels");

        let pixel_count = (self.image.width * self.image.height) as usize;
        let pixels = unsafe { std::slice::from_raw_parts(colors, pixel_count).to_vec() };
        unsafe { UnloadImageColors(colors) };
        pixels
    }

    // pub fn line(&mut self, x1: i32, y1: i32, x2: i32, y2: i32, color: Color) {
    //     unsafe { ImageDrawLine(&mut self.image, x1, y1, x2, y2, color) }
    // }

    // pub fn thick_line(&mut self, start: Vector2, end: Vector2, thick: i32, color: Color) {
    //     unsafe { ImageDrawLineEx(&mut self.image, start, end, thick, color) }
    // }

    pub fn circle(&mut self, x: i32, y: i32, radius: i32, color: Color) {
        unsafe { ImageDrawCircle(&mut self.image, x, y, radius, color) }
    }

    pub fn circle_outline(&mut self, x: i32, y: i32, radius: i32, color: Color) {
        unsafe { ImageDrawCircleLines(&mut self.image, x, y, radius, color) }
    }

    pub fn rect(&mut self, x: i32, y: i32, width: i32, height: i32, color: Color) {
        unsafe { ImageDrawRectangle(&mut self.image, x, y, width, height, color) }
    }

    pub fn rect_outline(
        &mut self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        thick: i32,
        color: Color,
    ) {
        let rec = Rectangle {
            x: x as f32,
            y: y as f32,
            width: width as f32,
            height: height as f32,
        };
        unsafe { ImageDrawRectangleLines(&mut self.image, rec, thick, color) }
    }

    // pub fn triangle(&mut self, a: Vector2, b: Vector2, c: Vector2, color: Color) {
    //     unsafe { ImageDrawTriangle(&mut self.image, a, b, c, color) }
    // }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        unsafe { UnloadImage(self.image) }
    }
}
