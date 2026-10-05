use std::ffi::c_void;

use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, SelectObject, SetBrushOrgEx, SetStretchBltMode,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HALFTONE, HBITMAP, HDC,
};

pub(crate) struct Dib {
    pub dc: HDC,
    pub bmp: HBITMAP,
    pub bits: *mut c_void,
}

pub(crate) unsafe fn make_dib(screen: HDC, w: i32, h: i32) -> Option<Dib> {
    let dc = CreateCompatibleDC(screen);
    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: w,
        biHeight: -h,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
    };
    let mut bits = std::ptr::null_mut();
    let bmp = CreateDIBSection(screen, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    SelectObject(dc, bmp);
    SetStretchBltMode(dc, HALFTONE);
    let _ = SetBrushOrgEx(dc, 0, 0, None);
    Some(Dib { dc, bmp, bits })
}

pub(crate) unsafe fn free_dib(d: Dib) {
    use windows::Win32::Graphics::Gdi::{DeleteDC, DeleteObject};
    let _ = DeleteObject(d.bmp);
    let _ = DeleteDC(d.dc);
}
