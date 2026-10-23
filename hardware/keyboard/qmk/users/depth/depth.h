// Depth keyboard userspace: the keycodes the Depth app listens for, and the status protocol it
// sends back over raw HID. Any QMK keyboard can use it: its keymap places the DK_ codes and says
// (optionally) which keys show the status. Spec: hardware/keyboard/README.md.
#pragma once
#include QMK_KEYBOARD_H

// What each repainted key sends: F13-F20 under Ctrl+Alt, Ctrl+Alt+Shift or Ctrl+Shift, which no OS
// or app uses by default. Must match the KEYBOARD table in the app (ui/src/hotkeys.rs; a test there
// checks every line below). Esc (CANCEL), Enter (GO) and / (CMD) keep their normal codes.
#define DK_HELP     C(A(KC_F13)) /* Help */
#define DK_SPOT     C(A(KC_F14)) /* Spot */
#define DK_MRGN     C(A(KC_F15)) /* Margin */
#define DK_PERP     C(A(KC_F16)) /* Perp */
#define DK_OPT      C(A(KC_F17)) /* Option */
#define DK_1M       C(A(KC_F18)) /* Tf1m */
#define DK_5M       C(A(KC_F19)) /* Tf5m */
#define DK_15M      C(A(KC_F20)) /* Tf15m */
#define DK_1H       C(A(S(KC_F13))) /* Tf1h */
#define DK_4H       C(A(S(KC_F14))) /* Tf4h */
#define DK_1D       C(A(S(KC_F15))) /* Tf1d */
#define DK_BACK     C(A(S(KC_F16))) /* Back */
#define DK_OPCL     C(A(S(KC_F17))) /* OpenClose */
#define DK_CXL      C(A(S(KC_F18))) /* CancelAll */
#define DK_FLAT     C(A(S(KC_F19))) /* Flatten */
#define DK_BUYBID   C(A(S(KC_F20))) /* BuyBid */
#define DK_BUYMKT   C(S(KC_F13)) /* BuyMarket */
#define DK_PGBACK   C(S(KC_F14)) /* PrevFav */
#define DK_SELLASK  C(S(KC_F15)) /* SellAsk */
#define DK_SELLMKT  C(S(KC_F16)) /* SellMarket */
#define DK_PGFWD    C(S(KC_F17)) /* NextFav */
#define DK_DIALUP   C(S(KC_F18)) /* DialUp */
#define DK_DIALDN   C(S(KC_F19)) /* DialDown */
#define DK_DIALMODE C(S(KC_F20)) /* DialMode */

// Raw HID (32-byte reports, usage page 0xFF60; with VIA on, through VIA's keyboard command hook, so
// VIA remapping keeps working). Host -> keyboard:
//   D7 00                      hello; the keyboard answers D7 00 <protocol version>
//   D7 01 <flags> <market> <tf> state, sent on change and at least every second
// flags: bit 0 live (connected, data fresh), 1 position held, 2 position in loss, 3 orders resting,
//        4 attention (a confirmation is waiting or an order was rejected), 5 order panel on Close.
// market: 0 spot, 1 margin, 2 perp, 3 option; tf: 0..5 = 1m 5m 15m 1h 4h 1d, 0xFF other.
// Without a state for DEPTH_TIMEOUT_MS the keyboard shows "not live" and stops highlighting.
#define DEPTH_MAGIC 0xD7
#define DEPTH_PROTOCOL 1
#ifndef DEPTH_TIMEOUT_MS
#    define DEPTH_TIMEOUT_MS 3000
#endif
enum { DK_F_LIVE = 1, DK_F_POS = 2, DK_F_LOSS = 4, DK_F_ORDERS = 8, DK_F_ATTN = 16, DK_F_CLOSE = 32 };

// A keymap may say where the status shows (matrix {row, col} of keys with RGB):
//   DEPTH_STATUS_KEYS { {r,c}, {r,c}, {r,c} }   LIVE, POS, ORD
//   DEPTH_MARKET_KEYS { 4 x {r,c} }  SPOT MRGN PERP OPT   DEPTH_TF_KEYS { 6 x {r,c} }  1m .. 1D
//   DEPTH_GO_KEY {r,c}   DEPTH_OPCL_KEY {r,c}
// Keyboards with discrete Num / Caps / Scroll Lock LEDs show LIVE / POS / ORD on them instead.

// state as last received (for keymaps that want more)
uint8_t depth_flags(void);
