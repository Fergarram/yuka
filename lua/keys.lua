--
-- Key names
--
-- Linux evdev codes (input-event-codes.h), which is what the engine reports. Names
-- are lowercase and the same ones a chord is written with: "super+w", "ctrl+alt+f1".
--

local keys = {
    escape = 1,
    ["1"] = 2, ["2"] = 3, ["3"] = 4, ["4"] = 5, ["5"] = 6,
    ["6"] = 7, ["7"] = 8, ["8"] = 9, ["9"] = 10, ["0"] = 11,
    minus = 12, equal = 13, backspace = 14, tab = 15,
    q = 16, w = 17, e = 18, r = 19, t = 20, y = 21, u = 22, i = 23, o = 24, p = 25,
    leftbrace = 26, rightbrace = 27, enter = 28, leftctrl = 29,
    a = 30, s = 31, d = 32, f = 33, g = 34, h = 35, j = 36, k = 37, l = 38,
    semicolon = 39, apostrophe = 40, grave = 41, leftshift = 42, backslash = 43,
    z = 44, x = 45, c = 46, v = 47, b = 48, n = 49, m = 50,
    comma = 51, dot = 52, slash = 53, rightshift = 54, kpasterisk = 55,
    leftalt = 56, space = 57, capslock = 58,
    f1 = 59, f2 = 60, f3 = 61, f4 = 62, f5 = 63, f6 = 64, f7 = 65, f8 = 66,
    f9 = 67, f10 = 68, f11 = 87, f12 = 88,
    numlock = 69, scrolllock = 70,
    kp7 = 71, kp8 = 72, kp9 = 73, kpminus = 74, kp4 = 75, kp5 = 76, kp6 = 77,
    kpplus = 78, kp1 = 79, kp2 = 80, kp3 = 81, kp0 = 82, kpdot = 83,
    kpenter = 96, rightctrl = 97, kpslash = 98, sysrq = 99, rightalt = 100,
    home = 102, up = 103, pageup = 104, left = 105, right = 106,
    ["end"] = 107, down = 108, pagedown = 109, insert = 110, delete = 111,
    leftmeta = 125, rightmeta = 126,
    hangeul = 122, hanja = 123,
    brightnessdown = 224, brightnessup = 225,
}

-- Shorter names for the ones chords are usually written with.
keys.esc = keys.escape
keys.plus = keys.equal
-- The JIS keys either side of the space bar on a Mac, which Linux reports under their
-- Korean names: 英数 as hanja, かな as hangeul.
keys.eisu = keys.hanja
keys.kana = keys.hangeul

return keys
