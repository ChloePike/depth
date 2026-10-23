// Depth keyboard userspace: raw HID status from the Depth app, shown on the lock LEDs and / or on
// RGB keys named by the keymap. See depth.h for the protocol.
#include "depth.h"
#include "raw_hid.h"

static uint8_t  dk_flags, dk_market = 0xFF, dk_tf = 0xFF;
static uint32_t dk_seen;

static bool fresh(void) { return dk_seen && timer_elapsed32(dk_seen) < DEPTH_TIMEOUT_MS; }
uint8_t depth_flags(void) { return fresh() ? dk_flags : 0; }

// Depth's reports start with 0xD7, a command id VIA does not use: with VIA on they arrive through its
// keyboard hook (VIA keeps working for remapping), without VIA through raw HID directly.
static bool depth_receive(uint8_t *data, uint8_t length) {
    if (length < 5 || data[0] != DEPTH_MAGIC) return false;
    if (data[1] == 0x00) {
        data[2] = DEPTH_PROTOCOL;
        raw_hid_send(data, length);
    } else if (data[1] == 0x01) {
        dk_flags  = data[2];
        dk_market = data[3];
        dk_tf     = data[4];
        dk_seen   = timer_read32() | 1;
    }
    return true;
}

#ifdef VIA_ENABLE
bool via_command_kb(uint8_t *data, uint8_t length) { return depth_receive(data, length); }
#else
void raw_hid_receive(uint8_t *data, uint8_t length) { depth_receive(data, length); }
#endif

// on, off or blinking with this half-period (ms)
static bool blink(uint16_t half) { return (timer_read() / half) & 1; }
static bool live(void) { return depth_flags() & DK_F_LIVE; }
static bool pos(void) { uint8_t f = depth_flags(); return (f & DK_F_POS) && (!(f & DK_F_LOSS) || blink(500)); }
static bool ord(void) { uint8_t f = depth_flags(); return (f & DK_F_ATTN) ? blink(150) : (f & DK_F_ORDERS) != 0; }

#if defined(LED_NUM_LOCK_PIN) && defined(LED_CAPS_LOCK_PIN) && defined(LED_SCROLL_LOCK_PIN)
#    ifndef LED_PIN_ON_STATE
#        define LED_PIN_ON_STATE 1
#    endif
// the three lock LEDs belong to Depth: LIVE, POS, ORD
bool led_update_user(led_t state) { return false; }
static void pin(pin_t p, bool on) { gpio_write_pin(p, on ? LED_PIN_ON_STATE : !LED_PIN_ON_STATE); }
void housekeeping_task_user(void) {
    pin(LED_NUM_LOCK_PIN, live());
    pin(LED_CAPS_LOCK_PIN, pos());
    pin(LED_SCROLL_LOCK_PIN, ord());
}
#endif

#ifdef RGB_MATRIX_ENABLE
static void key(uint8_t row, uint8_t col, uint8_t r, uint8_t g, uint8_t b, uint8_t lo, uint8_t hi) {
    uint8_t i = g_led_config.matrix_co[row][col];
    if (i != NO_LED && i >= lo && i < hi) rgb_matrix_set_color(i, r, g, b);
}
bool rgb_matrix_indicators_advanced_user(uint8_t lo, uint8_t hi) {
#    ifdef DEPTH_STATUS_KEYS
    static const uint8_t st[3][2] = DEPTH_STATUS_KEYS;
    key(st[0][0], st[0][1], 0, live() ? 160 : 0, 0, lo, hi);
    key(st[1][0], st[1][1], pos() ? 200 : 0, pos() ? 150 : 0, 0, lo, hi);
    key(st[2][0], st[2][1], 0, ord() ? 90 : 0, ord() ? 220 : 0, lo, hi);
#    endif
    if (!fresh()) return true;
#    ifdef DEPTH_MARKET_KEYS
    static const uint8_t mk[4][2] = DEPTH_MARKET_KEYS;
    if (dk_market < 4) key(mk[dk_market][0], mk[dk_market][1], 255, 190, 0, lo, hi);
#    endif
#    ifdef DEPTH_TF_KEYS
    static const uint8_t tf[6][2] = DEPTH_TF_KEYS;
    if (dk_tf < 6) key(tf[dk_tf][0], tf[dk_tf][1], 255, 255, 255, lo, hi);
#    endif
#    ifdef DEPTH_GO_KEY
    static const uint8_t go[2] = DEPTH_GO_KEY;
    if ((dk_flags & DK_F_ATTN) && blink(150)) key(go[0], go[1], 0, 255, 60, lo, hi);
#    endif
#    ifdef DEPTH_OPCL_KEY
    static const uint8_t oc[2] = DEPTH_OPCL_KEY;
    if (dk_flags & DK_F_CLOSE) key(oc[0], oc[1], 255, 120, 0, lo, hi);
#    endif
    return true;
}
#endif
