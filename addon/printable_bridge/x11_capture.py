"""Read visible editor pixels from the supervisor-owned X11 display."""

import ctypes as c


class DisplayCaptureError(RuntimeError):
    pass


class _XImage(c.Structure):
    _fields_ = [
        ("width", c.c_int), ("height", c.c_int), ("xoffset", c.c_int),
        ("format", c.c_int), ("data", c.c_void_p), ("byte_order", c.c_int),
        ("bitmap_unit", c.c_int), ("bitmap_bit_order", c.c_int),
        ("bitmap_pad", c.c_int), ("depth", c.c_int),
        ("bytes_per_line", c.c_int), ("bits_per_pixel", c.c_int),
        ("red_mask", c.c_ulong), ("green_mask", c.c_ulong), ("blue_mask", c.c_ulong),
    ]


def editor_rectangle(window, area, display_width, display_height):
    if (area.width <= 0 or area.height <= 0 or area.width * area.height > 8388608
            or max(area.width, area.height) > 4096):
        raise DisplayCaptureError("editor capture exceeds its source pixel budget")
    if (area.x < 0 or area.y < 0 or area.x + area.width > window.width
            or area.y + area.height > window.height):
        raise DisplayCaptureError("editor does not fit the selected window at native pixel scale")
    left = window.x + area.x
    top = display_height - window.y - area.y - area.height
    if (left < 0 or top < 0 or left + area.width > display_width
            or top + area.height > display_height):
        raise DisplayCaptureError("selected editor is outside the private display")
    return left, top, area.width, area.height


def rgb_pixels(raw, width, height, stride):
    if stride < width * 4 or len(raw) != stride * height:
        raise DisplayCaptureError("private display returned invalid pixel storage")
    output = bytearray(width * height * 3)
    for row in range(height):
        source = memoryview(raw)[row * stride:row * stride + width * 4]
        offset = row * width * 3
        output[offset:offset + width * 3:3] = source[2::4]
        output[offset + 1:offset + width * 3:3] = source[1::4]
        output[offset + 2:offset + width * 3:3] = source[0::4]
    return bytes(output)


def capture_rgb(window, area):
    try:
        x = c.CDLL("libX11.so.6")
    except OSError as error:
        raise DisplayCaptureError("native editor capture requires the private X11 runtime") from error
    x.XOpenDisplay.argtypes = [c.c_char_p]
    x.XOpenDisplay.restype = c.c_void_p
    x.XDefaultRootWindow.argtypes = [c.c_void_p]
    x.XDefaultRootWindow.restype = c.c_ulong
    for name in ("XDisplayWidth", "XDisplayHeight"):
        function = getattr(x, name)
        function.argtypes = [c.c_void_p, c.c_int]
        function.restype = c.c_int
    x.XGetImage.argtypes = [c.c_void_p, c.c_ulong, c.c_int, c.c_int,
                           c.c_uint, c.c_uint, c.c_ulong, c.c_int]
    x.XGetImage.restype = c.POINTER(_XImage)
    x.XDestroyImage.argtypes = [c.POINTER(_XImage)]
    x.XCloseDisplay.argtypes = [c.c_void_p]
    display = x.XOpenDisplay(None)
    if not display:
        raise DisplayCaptureError("private X11 display is unavailable")
    try:
        rectangle = editor_rectangle(window, area, x.XDisplayWidth(display, 0),
                                     x.XDisplayHeight(display, 0))
        left, top, width, height = rectangle
        image = x.XGetImage(display, x.XDefaultRootWindow(display), left, top,
                            width, height, c.c_ulong(-1), 2)
        if not image:
            raise DisplayCaptureError("private display did not return editor pixels")
        try:
            pixels = image.contents
            if (not pixels.data or pixels.width != width or pixels.height != height or pixels.depth != 24
                    or pixels.bits_per_pixel != 32 or pixels.byte_order != 0
                    or (pixels.red_mask, pixels.green_mask, pixels.blue_mask)
                    != (0xff0000, 0x00ff00, 0x0000ff)
                    or not width * 4 <= pixels.bytes_per_line <= width * 4 + 64):
                raise DisplayCaptureError("private display pixel format is unsupported")
            raw = c.string_at(pixels.data, pixels.bytes_per_line * height)
            return rgb_pixels(raw, width, height, pixels.bytes_per_line), width, height
        finally:
            x.XDestroyImage(image)
    finally:
        x.XCloseDisplay(display)
