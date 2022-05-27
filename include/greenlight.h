#ifndef GDK_GREENLIGHT_H
#define GDK_GREENLIGHT_H
#pragma once

#include "gdk.h"

#ifdef __cplusplus
extern "C" {
#endif

GDK_API int GA_gl_close(struct GA_session* session, const GA_json* params, GA_json** output);
GDK_API int GA_gl_connect(struct GA_session* session, const GA_json* params, GA_json** output);
GDK_API int GA_gl_destroy(struct GA_session* session, const GA_json* params, GA_json** output);
GDK_API int GA_gl_disconnect(struct GA_session* session, const GA_json* params, GA_json** output);
GDK_API int GA_gl_fundchannel(struct GA_session* session, const GA_json* params, GA_json** output);
GDK_API int GA_gl_getinfo(struct GA_session* session, const GA_json* params, GA_json** output);
GDK_API int GA_gl_hsmd(struct GA_session* session, const GA_json* params, GA_json** output);
GDK_API int GA_gl_invoice(struct GA_session* session, const GA_json* params, GA_json** output);
GDK_API int GA_gl_listfunds(struct GA_session* session, const GA_json* params, GA_json** output);
GDK_API int GA_gl_listpeers(struct GA_session* session, const GA_json* params, GA_json** output);
GDK_API int GA_gl_newaddr(struct GA_session* session, const GA_json* params, GA_json** output);
GDK_API int GA_gl_pay(struct GA_session* session, const GA_json* params, GA_json** output);
GDK_API int GA_gl_scheduler(struct GA_session* session, const GA_json* params, GA_json** output);
GDK_API int GA_gl_stop(struct GA_session* session, const GA_json* params, GA_json** output);
GDK_API int GA_gl_withdraw(struct GA_session* session, const GA_json* params, GA_json** output);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* GDK_GREENLIGHT_H */
