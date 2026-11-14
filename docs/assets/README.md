# Screenshot Replacement & Privacy Specification

This directory contains interface preview screenshots used across project documentation.

## Current Screenshot Inventory & Usage

| File | Size (Raw) | Referenced In | Content Description |
| :--- | :--- | :--- | :--- |
| `main-page.png` | ~5.6 MB | [README.md](../../README.md#L27) | Main overview: device status, battery level, mirror and webcam controls |
| `apps-page.png` | ~5.6 MB | [README.md](../../README.md#L27) | App streaming page: list of Android applications launchable on Linux |
| `display-page.png` | ~5.6 MB | [README.md](../../README.md#L30) | Cross Control & display streaming: edge traversal, mirror, extend monitor |
| `files-page.png` | ~5.8 MB | [README.md](../../README.md#L30) | Wireless storage: phone storage mount, PC folder share, status toggles |

---

## Privacy & Sanitization Requirements for New Captures

The original captures contained uncompressed full-resolution bitmaps (22.8 MB total) and exposed personal metadata. Replacement captures must strictly adhere to the following sanitization rules:

1. **Device Identification**:
   - Do **not** show real hardware device names or serials (e.g. use generic labels like `"Motorola Phone"` or `"Android Device"`).
   - Ensure Bluetooth MAC addresses and local IP addresses are either obscured or set to standard test ranges (e.g. `192.168.1.100` / `00:11:22:33:44:55`).

2. **Application List (`apps-page.png`)**:
   - Do **not** display personal, proprietary, or financial applications.
   - Use standard system/AOSP or open-source utility apps:
     - Settings, Files, Calculator, Clock, Calendar, Camera, Gallery, Contacts, Media Player.

3. **Storage & File Paths (`files-page.png`)**:
   - Do **not** display personal usernames or home directory paths (e.g. avoid `/home/<username>/...`).
   - Use generic shared paths such as `/home/user/Shared` or default system folders (`Downloads`, `Documents`, `Music`).
   - Ensure thumbnail previews do not display personal photographs or identifiable documents.

4. **Display & Wallpaper (`display-page.png` & `main-page.png`)**:
   - Use default desktop wallpapers (e.g., standard GNOME/Adwaita background) without personal icons on the desktop.

---

## Technical Specifications & Optimization

- **Max File Size**: Under **300 KB** per image (total directory size < 1.2 MB).
- **Format**: PNG (8-bit color palette or optimized RGBA).
- **Target Dimensions**: Window-cropped captures around 900×700 to 1200×900 px (avoid full 4K screen captures).

### Recommended Optimization Commands

Using `pngquant` (lossy quantizer, ~80% size reduction with imperceptible visual loss):
```bash
pngquant --quality=80-90 --strip --ext .png --force docs/assets/*.png
```

Using `oxipng` (lossless optimization):
```bash
oxipng -o 4 --strip all docs/assets/*.png
```

Verification command:
```bash
ls -lh docs/assets/*.png
# Ensure each file is < 300 KB
```
