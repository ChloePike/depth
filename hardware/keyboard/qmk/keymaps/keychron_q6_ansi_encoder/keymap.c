// Depth keyboard on the Keychron Q6 (ANSI, knob). Built as keyboards/keychron/q6/ansi_encoder/keymaps/depth,
// which pulls in users/depth. Base layers: the repainted keys send the Depth codes; Fn gives back
// F1-F12, PrtSc / ScrLk / Pause and the edit cluster. The four keys over the numpad show LIVE / POS /
// ORD (config.h); the knob dials size or price, its push switches between them.
#include "depth.h"

// clang-format off
enum layers { MAC_BASE, MAC_FN, WIN_BASE, WIN_FN };

const uint16_t PROGMEM keymaps[][MATRIX_ROWS][MATRIX_COLS] = {
    [MAC_BASE] = LAYOUT_ansi_109(
        KC_ESC,   DK_HELP,  DK_SPOT,  DK_MRGN,  DK_PERP,  DK_OPT,   DK_1M,    DK_5M,    DK_15M,   DK_1H,    DK_4H,    DK_1D,    DK_BACK,    DK_DIALMODE, DK_OPCL, DK_CXL, DK_FLAT, KC_NO,  KC_NO,    KC_NO,    KC_NO,
        KC_GRV,   KC_1,     KC_2,     KC_3,     KC_4,     KC_5,     KC_6,     KC_7,     KC_8,     KC_9,     KC_0,     KC_MINS,  KC_EQL,     KC_BSPC,  DK_BUYBID, DK_BUYMKT, DK_PGBACK, KC_NUM, KC_PSLS, KC_PAST, KC_PMNS,
        KC_TAB,   KC_Q,     KC_W,     KC_E,     KC_R,     KC_T,     KC_Y,     KC_U,     KC_I,     KC_O,     KC_P,     KC_LBRC,  KC_RBRC,    KC_BSLS,  DK_SELLASK, DK_SELLMKT, DK_PGFWD, KC_P7, KC_P8, KC_P9,
        KC_CAPS,  KC_A,     KC_S,     KC_D,     KC_F,     KC_G,     KC_H,     KC_J,     KC_K,     KC_L,     KC_SCLN,  KC_QUOT,              KC_ENT,                                 KC_P4,    KC_P5,    KC_P6,    KC_PPLS,
        KC_LSFT,            KC_Z,     KC_X,     KC_C,     KC_V,     KC_B,     KC_N,     KC_M,     KC_COMM,  KC_DOT,   KC_SLSH,              KC_RSFT,            KC_UP,              KC_P1,    KC_P2,    KC_P3,
        KC_LCTL,  KC_LOPT,  KC_LCMD,                                KC_SPC,                                 KC_RCMD,  KC_ROPT,  MO(MAC_FN), KC_RCTL,  KC_LEFT,  KC_DOWN,  KC_RGHT,  KC_P0,              KC_PDOT,  KC_PENT),
    [MAC_FN] = LAYOUT_ansi_109(
        _______,  KC_F1,    KC_F2,    KC_F3,    KC_F4,    KC_F5,    KC_F6,    KC_F7,    KC_F8,    KC_F9,    KC_F10,   KC_F11,   KC_F12,     KC_MUTE,  KC_PSCR,  KC_SCRL,  KC_PAUS,  KC_F13,   KC_F14,   KC_F15,   KC_F16,
        _______,  _______,  _______,  _______,  _______,  _______,  _______,  _______,  _______,  _______,  _______,  _______,  _______,    _______,  KC_INS,   KC_HOME,  KC_PGUP,  _______,  _______,  _______,  _______,
        RM_TOGG,  RM_NEXT,  RM_VALU,  RM_HUEU,  RM_SATU,  RM_SPDU,  _______,  _______,  _______,  _______,  _______,  _______,  _______,    _______,  KC_DEL,   KC_END,   KC_PGDN,  _______,  _______,  _______,
        _______,  RM_PREV,  RM_VALD,  RM_HUED,  RM_SATD,  RM_SPDD,  _______,  _______,  _______,  _______,  _______,  _______,              _______,                                _______,  _______,  _______,  _______,
        _______,            _______,  _______,  _______,  _______,  _______,  NK_TOGG,  _______,  _______,  _______,  _______,              _______,            _______,            _______,  _______,  _______,
        _______,  _______,  _______,                                _______,                                _______,  _______,  _______,    _______,  _______,  _______,  _______,  _______,            _______,  _______),
    [WIN_BASE] = LAYOUT_ansi_109(
        KC_ESC,   DK_HELP,  DK_SPOT,  DK_MRGN,  DK_PERP,  DK_OPT,   DK_1M,    DK_5M,    DK_15M,   DK_1H,    DK_4H,    DK_1D,    DK_BACK,    DK_DIALMODE, DK_OPCL, DK_CXL, DK_FLAT, KC_NO,  KC_NO,    KC_NO,    KC_NO,
        KC_GRV,   KC_1,     KC_2,     KC_3,     KC_4,     KC_5,     KC_6,     KC_7,     KC_8,     KC_9,     KC_0,     KC_MINS,  KC_EQL,     KC_BSPC,  DK_BUYBID, DK_BUYMKT, DK_PGBACK, KC_NUM, KC_PSLS, KC_PAST, KC_PMNS,
        KC_TAB,   KC_Q,     KC_W,     KC_E,     KC_R,     KC_T,     KC_Y,     KC_U,     KC_I,     KC_O,     KC_P,     KC_LBRC,  KC_RBRC,    KC_BSLS,  DK_SELLASK, DK_SELLMKT, DK_PGFWD, KC_P7, KC_P8, KC_P9,
        KC_CAPS,  KC_A,     KC_S,     KC_D,     KC_F,     KC_G,     KC_H,     KC_J,     KC_K,     KC_L,     KC_SCLN,  KC_QUOT,              KC_ENT,                                 KC_P4,    KC_P5,    KC_P6,    KC_PPLS,
        KC_LSFT,            KC_Z,     KC_X,     KC_C,     KC_V,     KC_B,     KC_N,     KC_M,     KC_COMM,  KC_DOT,   KC_SLSH,              KC_RSFT,            KC_UP,              KC_P1,    KC_P2,    KC_P3,
        KC_LCTL,  KC_LWIN,  KC_LALT,                                KC_SPC,                                 KC_RALT,  KC_RWIN,  MO(WIN_FN), KC_RCTL,  KC_LEFT,  KC_DOWN,  KC_RGHT,  KC_P0,              KC_PDOT,  KC_PENT),
    [WIN_FN] = LAYOUT_ansi_109(
        _______,  KC_F1,    KC_F2,    KC_F3,    KC_F4,    KC_F5,    KC_F6,    KC_F7,    KC_F8,    KC_F9,    KC_F10,   KC_F11,   KC_F12,     KC_MUTE,  KC_PSCR,  KC_SCRL,  KC_PAUS,  KC_F13,   KC_F14,   KC_F15,   KC_F16,
        _______,  _______,  _______,  _______,  _______,  _______,  _______,  _______,  _______,  _______,  _______,  _______,  _______,    _______,  KC_INS,   KC_HOME,  KC_PGUP,  _______,  _______,  _______,  _______,
        RM_TOGG,  RM_NEXT,  RM_VALU,  RM_HUEU,  RM_SATU,  RM_SPDU,  _______,  _______,  _______,  _______,  _______,  _______,  _______,    _______,  KC_DEL,   KC_END,   KC_PGDN,  _______,  _______,  _______,
        _______,  RM_PREV,  RM_VALD,  RM_HUED,  RM_SATD,  RM_SPDD,  _______,  _______,  _______,  _______,  _______,  _______,              _______,                                _______,  _______,  _______,  _______,
        _______,            _______,  _______,  _______,  _______,  _______,  NK_TOGG,  _______,  _______,  _______,  _______,              _______,            _______,            _______,  _______,  _______,
        _______,  _______,  _______,                                _______,                                _______,  _______,  _______,    _______,  _______,  _______,  _______,  _______,            _______,  _______),
};

#if defined(ENCODER_MAP_ENABLE)
const uint16_t PROGMEM encoder_map[][NUM_ENCODERS][NUM_DIRECTIONS] = {
    [MAC_BASE] = {ENCODER_CCW_CW(DK_DIALDN, DK_DIALUP)},
    [MAC_FN]   = {ENCODER_CCW_CW(KC_VOLD, KC_VOLU)},
    [WIN_BASE] = {ENCODER_CCW_CW(DK_DIALDN, DK_DIALUP)},
    [WIN_FN]   = {ENCODER_CCW_CW(KC_VOLD, KC_VOLU)},
};
#endif
