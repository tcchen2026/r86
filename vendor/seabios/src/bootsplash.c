// Initialize the VGA console and possibly show a boot splash image.
//
// Copyright (C) 2009-2010  coresystems GmbH
// Copyright (C) 2010  Kevin O'Connor <kevin@koconnor.net>
//
// This file may be distributed under the terms of the GNU LGPLv3 license.

#include "bregs.h" // struct bregs
#include "config.h" // CONFIG_*
#include "farptr.h" // FLATPTR_TO_SEG
#include "malloc.h" // free
#include "output.h" // dprintf
#include "romfile.h" // romfile_loadfile
#include "stacks.h" // call16_int
#include "std/vbe.h" // struct vbe_info
#include "string.h" // memset
#include "util.h" // enable_bootsplash
#include "x86.h" // outb


/****************************************************************
 * Helper functions
 ****************************************************************/

// Call int10 vga handler.
static void
call16_int10(struct bregs *br)
{
    br->flags = F_IF;
    start_preempt();
    call16_int(0x10, br);
    finish_preempt();
}


/****************************************************************
 * VGA text / graphics console
 ****************************************************************/

// r86's active engine occupies the top-right corner without moving the
// console cursor. Mode 3 has 80 columns; mode 13h has 40.
static void
r86_show_engine(int columns)
{
    int size;
    char *label = romfile_loadfile("etc/r86-engine", &size);
    if (!label || size < 1 || size > columns) {
        free(label);
        return;
    }
    struct bregs br;
    memset(&br, 0, sizeof(br));
    br.ah = 0x03;
    call16_int10(&br);
    u16 cursor = br.dx;
    memset(&br, 0, sizeof(br));
    br.ah = 0x02;
    br.dx = columns - size;
    call16_int10(&br);
    printf("%s", label);
    memset(&br, 0, sizeof(br));
    br.ah = 0x02;
    br.dx = cursor;
    call16_int10(&br);
    free(label);
}

void
enable_vga_console(void)
{
    dprintf(1, "Turning on vga text mode console\n");
    struct bregs br;

    /* Enable VGA text mode */
    memset(&br, 0, sizeof(br));
    br.ax = 0x0003;
    call16_int10(&br);

    // Write to screen.
    printf("SeaBIOS (version %s)\n", VERSION);
    r86_show_engine(80);
    display_uuid();
}

static int
find_videomode(struct vbe_info *vesa_info, struct vbe_mode_info *mode_info
               , int width, int height, int bpp_req)
{
    dprintf(3, "Finding vesa mode with dimensions %d/%d\n", width, height);
    u16 *videomodes = SEGOFF_TO_FLATPTR(vesa_info->video_mode);
    for (;; videomodes++) {
        u16 videomode = *videomodes;
        if (videomode == 0xffff) {
            dprintf(1, "Unable to find vesa video mode dimensions %d/%d\n"
                    , width, height);
            return -1;
        }
        struct bregs br;
        memset(&br, 0, sizeof(br));
        br.ax = 0x4f01;
        br.cx = videomode;
        br.di = FLATPTR_TO_OFFSET(mode_info);
        br.es = FLATPTR_TO_SEG(mode_info);
        call16_int10(&br);
        if (br.ax != 0x4f) {
            dprintf(1, "get_mode failed.\n");
            continue;
        }
        if (mode_info->xres != width
            || mode_info->yres != height)
            continue;
        u8 depth = mode_info->bits_per_pixel;
        if (bpp_req == 0) {
            if ((depth != 16 && depth != 24 && depth != 32)
                || mode_info->green_size == 5)
                continue;
        } else {
            if (depth != bpp_req)
                continue;
        }
        return videomode;
    }
}

static int BootsplashActive;

void
enable_bootsplash(void)
{
    if (!CONFIG_BOOTSPLASH)
        return;
    /* splash picture can be bmp or jpeg file */
    dprintf(3, "Checking for bootsplash\n");
    u8 type = 0; /* 0 means jpg, 1 means bmp, default is 0=jpg */
    int filesize;
    u8 *filedata = romfile_loadfile("bootsplash.jpg", &filesize);
    if (!filedata) {
        filedata = romfile_loadfile("bootsplash.bmp", &filesize);
        if (!filedata)
            return;
        type = 1;
    }
    dprintf(3, "start showing bootsplash\n");

    u8 *picture = NULL; /* data buff used to be flushed to the video buf */
    struct jpeg_decdata *jpeg = NULL;
    struct bmp_decdata *bmp = NULL;
    struct vbe_info *vesa_info = malloc_tmplow(sizeof(*vesa_info));
    struct vbe_mode_info *mode_info = malloc_tmplow(sizeof(*mode_info));
    if (!vesa_info || !mode_info) {
        warn_noalloc();
        goto done;
    }

    /* Check whether we have a VESA 2.0 compliant BIOS */
    memset(vesa_info, 0, sizeof(struct vbe_info));
    vesa_info->signature = VBE2_SIGNATURE;
    struct bregs br;
    memset(&br, 0, sizeof(br));
    br.ax = 0x4f00;
    br.di = FLATPTR_TO_OFFSET(vesa_info);
    br.es = FLATPTR_TO_SEG(vesa_info);
    call16_int10(&br);
    if (vesa_info->signature != VESA_SIGNATURE) {
        dprintf(1,"No VBE2 found.\n");
        goto done;
    }

    /* Print some debugging information about our card. */
    char *vendor = SEGOFF_TO_FLATPTR(vesa_info->oem_vendor_string);
    char *product = SEGOFF_TO_FLATPTR(vesa_info->oem_product_string);
    dprintf(3, "VESA %d.%d\nVENDOR: %s\nPRODUCT: %s\n",
            vesa_info->version>>8, vesa_info->version&0xff,
            vendor, product);

    int ret, width, height;
    int bpp_require = 0;
    if (type == 0) {
        jpeg = jpeg_alloc();
        if (!jpeg) {
            warn_noalloc();
            goto done;
        }
        /* Parse jpeg and get image size. */
        dprintf(5, "Decoding bootsplash.jpg\n");
        ret = jpeg_decode(jpeg, filedata);
        if (ret) {
            dprintf(1, "jpeg_decode failed with return code %d...\n", ret);
            goto done;
        }
        jpeg_get_size(jpeg, &width, &height);
    } else {
        bmp = bmp_alloc();
        if (!bmp) {
            warn_noalloc();
            goto done;
        }
        /* Parse bmp and get image size. */
        dprintf(5, "Decoding bootsplash.bmp\n");
        ret = bmp_decode(bmp, filedata, filesize);
        if (ret) {
            dprintf(1, "bmp_decode failed with return code %d...\n", ret);
            goto done;
        }
        bmp_get_info(bmp, &width, &height, &bpp_require);
    }

    // jpeg would use 16 or 24 bpp video mode, BMP uses 16/24/32 bpp mode.

    // Try to find a graphics mode with the corresponding dimensions.
    int videomode = find_videomode(vesa_info, mode_info, width, height,
                                       bpp_require);
    if (videomode < 0) {
        dprintf(1, "failed to find a videomode with %dx%d %dbpp (0=any).\n",
                    width, height, bpp_require);
        goto done;
    }
    void *framebuffer = (void *)mode_info->phys_base;
    int depth = mode_info->bits_per_pixel;
    dprintf(3, "mode: %04x\n", videomode);
    dprintf(3, "framebuffer: %p\n", framebuffer);
    dprintf(3, "bytes per scanline: %d\n", mode_info->bytes_per_scanline);
    dprintf(3, "bits per pixel: %d\n", depth);

    // Allocate space for image and decompress it.
    int imagesize = height * mode_info->bytes_per_scanline;
    picture = malloc_tmphigh(imagesize);
    if (!picture) {
        warn_noalloc();
        goto done;
    }

    if (type == 0) {
        dprintf(5, "Decompressing bootsplash.jpg\n");
        ret = jpeg_show(jpeg, picture, width, height, depth,
                            mode_info->bytes_per_scanline);
        if (ret) {
            dprintf(1, "jpeg_show failed with return code %d...\n", ret);
            goto done;
        }
    } else {
        dprintf(5, "Decompressing bootsplash.bmp\n");
        ret = bmp_show(bmp, picture, width, height, depth,
                           mode_info->bytes_per_scanline);
        if (ret) {
            dprintf(1, "bmp_show failed with return code %d...\n", ret);
            goto done;
        }
    }

    /* Switch to graphics mode */
    dprintf(5, "Switching to graphics mode\n");
    memset(&br, 0, sizeof(br));
    br.ax = 0x4f02;
    br.bx = videomode | VBE_MODE_LINEAR_FRAME_BUFFER;
    call16_int10(&br);
    if (br.ax != 0x4f) {
        dprintf(1, "set_mode failed.\n");
        goto done;
    }

    /* Show the picture */
    dprintf(5, "Showing bootsplash picture\n");
    iomemcpy(framebuffer, picture, imagesize);
    dprintf(5, "Bootsplash copy complete\n");
    BootsplashActive = 1;

done:
    free(filedata);
    free(picture);
    free(vesa_info);
    free(mode_info);
    free(jpeg);
    free(bmp);
    return;
}

void
disable_bootsplash(void)
{
    if (!CONFIG_BOOTSPLASH || !BootsplashActive)
        return;
    BootsplashActive = 0;
    enable_vga_console();
}


/****************************************************************
 * r86 splash
 ****************************************************************/

#define R86_SCALE 3
#define R86_DAC 0xf0 // the logo's colours replace the palette's last entries

// Fill a rectangle of the VGA mode 13h screen.
static void
r86_fill(int x, int y, int w, int h, u8 colour)
{
    u8 *row = (u8 *)0xa0000 + y * 320 + x;
    for (; h > 0; h--, row += 320)
        memset(row, colour, w);
}

// Wait up to `msec` for a key like get_keystroke(), showing r86's logo, the
// boot menu hint and a bar stepping 10-30-70-100% in VGA mode 13h, then
// return to text mode. r86 supplies the logo as etc/r86-splash: u16 width,
// u16 height, u8 colour count, 8-bit RGB per colour, then one colour per
// pixel (0 background, 1 also fills the bar). Without it the wait is plain.
int
r86_splash(int msec)
{
    static const u8 steps[] = { 10, 30, 70, 100 };
    int size, key = -1;
    u8 *file = romfile_loadfile("etc/r86-splash", &size);
    int width = 0, height = 0, colours = 0;
    if (file && size >= 5) {
        width = file[0] | file[1] << 8;
        height = file[2] | file[3] << 8;
        colours = file[4];
    }
    if (colours < 2 || colours > 16 || width * R86_SCALE > 320
        || height * R86_SCALE > 120 || 5 + colours * 3 + width * height > size) {
        free(file);
        return get_keystroke(msec);
    }
    u8 *rgb = file + 5, *pixels = rgb + colours * 3;

    struct bregs br;
    memset(&br, 0, sizeof(br));
    br.ax = 0x0013;
    call16_int10(&br);
    int i, x, y;
    outb(R86_DAC, 0x3c8);
    for (i = 0; i < colours * 3; i++)
        outb(rgb[i] >> 2, 0x3c9);
    int left = (320 - width * R86_SCALE) / 2;
    for (y = 0; y < height; y++)
        for (x = 0; x < width; x++) {
            u8 c = pixels[y * width + x];
            if (c)
                r86_fill(left + x * R86_SCALE, 24 + y * R86_SCALE
                         , R86_SCALE, R86_SCALE, R86_DAC + c);
        }
    free(file);
    r86_show_engine(40);

    // The VGA BIOS font in colour 7 (light grey), row 17, column 8.
    memset(&br, 0, sizeof(br));
    br.ah = 0x02;
    br.dx = (17 << 8) | 8;
    call16_int10(&br);
    printf("Press ESC for boot menu");
    r86_fill(59, 159, 202, 10, 8);
    r86_fill(60, 160, 200, 8, 0);
    for (i = 0; i < ARRAY_SIZE(steps) && key < 0; i++) {
        r86_fill(60, 160, 200 * steps[i] / 100, 8, R86_DAC + 1);
        key = get_keystroke(msec / ARRAY_SIZE(steps));
    }

    memset(&br, 0, sizeof(br));
    br.ax = 0x0003;
    call16_int10(&br);
    r86_show_engine(80);
    return key;
}
