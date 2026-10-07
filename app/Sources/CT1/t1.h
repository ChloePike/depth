// C ABI of t1-ffi (Rust staticlib). All calls on the main thread.
// Coordinates in points, top-left origin. mods: 1 shift, 2 ctrl, 4 alt, 8 cmd.
#pragma once
#include <stdint.h>

typedef struct T1View T1View;

T1View *t1_view_new(void *ns_view, float w, float h, float scale);
void t1_view_free(T1View *v);
void t1_view_resize(T1View *v, float w, float h, float scale);
int32_t t1_view_render(T1View *v);
void t1_view_pointer_move(T1View *v, float x, float y);
void t1_view_pointer_leave(T1View *v);
void t1_view_pointer_button(T1View *v, float x, float y, int32_t button, int32_t pressed, uint32_t mods);
void t1_view_scroll(T1View *v, float dx, float dy, uint32_t mods);
void t1_view_zoom(T1View *v, float factor);
void t1_view_key(T1View *v, const char *name, int32_t pressed, uint32_t mods);
void t1_view_text(T1View *v, const char *text);
void t1_view_paste(T1View *v, const char *text);
void t1_view_focus(T1View *v, int32_t focused);
int32_t t1_view_cursor(T1View *v);
char *t1_view_take_copied(T1View *v);
// native panels: state snapshot / one action, both JSON; free results with t1_free
char *t1_state(T1View *v);
char *t1_call(T1View *v, const char *req);
void t1_free(char *s);
