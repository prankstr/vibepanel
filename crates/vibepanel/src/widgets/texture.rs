//! Texture construction helpers.
//!
//! `gdk::Texture::for_pixbuf` is deprecated since GTK 4.20; these build a
//! `gdk::MemoryTexture` from the pixel data instead, which is what GTK does
//! internally.

use gtk4::gdk;
use gtk4::gdk_pixbuf::Pixbuf;
use gtk4::glib;
use gtk4::prelude::*;

/// Build a texture from a `Pixbuf`.
///
/// Pixbufs are always 8 bits per sample, with straight (non-premultiplied)
/// alpha when they have an alpha channel.
pub(crate) fn texture_for_pixbuf(pixbuf: &Pixbuf) -> gdk::Texture {
    let format = if pixbuf.has_alpha() {
        gdk::MemoryFormat::R8g8b8a8
    } else {
        gdk::MemoryFormat::R8g8b8
    };
    gdk::MemoryTexture::new(
        pixbuf.width(),
        pixbuf.height(),
        format,
        &pixbuf.read_pixel_bytes(),
        pixbuf.rowstride() as usize,
    )
    .upcast()
}

/// Build a texture from tightly packed, straight-alpha RGBA bytes.
pub(crate) fn texture_from_rgba(rgba: Vec<u8>, width: i32, height: i32) -> gdk::Texture {
    gdk::MemoryTexture::new(
        width,
        height,
        gdk::MemoryFormat::R8g8b8a8,
        &glib::Bytes::from_owned(rgba),
        width as usize * 4,
    )
    .upcast()
}
