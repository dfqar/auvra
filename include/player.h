/* Generated from player-ffi. Do not edit by hand. */

#ifndef AUVRA_PLAYER_H
#define AUVRA_PLAYER_H

#pragma once

#include <stdarg.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>

#define PLAYER_ABI_VERSION 2

enum PlayerResult
#if __STDC_VERSION__ >= 202311L
  : int32_t
#endif // __STDC_VERSION__ >= 202311L
 {
  PlayerResult_Ok = 0,
  PlayerResult_InvalidArgument = 1,
  PlayerResult_BufferTooSmall = 2,
  PlayerResult_Empty = 3,
  PlayerResult_Unavailable = 4,
  PlayerResult_Panic = 5,
  PlayerResult_Busy = 6,
};
#if __STDC_VERSION__ >= 202311L
typedef enum PlayerResult PlayerResult;
#else
typedef int32_t PlayerResult;
#endif // __STDC_VERSION__ >= 202311L

typedef struct PlayerHandle PlayerHandle;

uint32_t player_abi_version(void);

/**
 * # Safety
 * `out` must be writable. A returned handle has one serial caller and must be destroyed once.
 */
PlayerResult player_create(struct PlayerHandle **out);

/**
 * # Safety
 * Null or a live unique handle. No calls may race or follow destruction.
 * Stops and joins the coordinator worker; host calls this on its control queue.
 */
void player_destroy(struct PlayerHandle *handle);

/**
 * # Safety
 * Live serially-owned handle; `bytes` points to `length` readable bytes.
 * Ok means accepted, not executed. Inspect command_result with the same request_id.
 * Busy/InvalidArgument/Unavailable reject without executing or producing a result.
 */
PlayerResult player_send_json(struct PlayerHandle *handle, const uint8_t *bytes, size_t length);

/**
 * # Safety
 * `out_length` is writable; non-null `buffer` has `capacity` writable bytes.
 * Null/short buffer pins this exact event and reports its size without consuming it.
 * A successful copy consumes it and releases command-result credit. JSON has no trailing NUL.
 */
PlayerResult player_event_drain(struct PlayerHandle *handle,
                                uint8_t *buffer,
                                size_t capacity,
                                size_t *out_length);

/**
 * Pure lyrics normalization, independent of engine handles. Call only on a host IO worker.
 * Input is {format,text,duration_ms}; output is bounded UTF-8 JSON, no trailing NUL.
 * Null/short output returns the required size. Repeating the same input is deterministic.
 * # Safety
 * `bytes` has `length` readable bytes, `out_length` is writable, and non-null
 * `buffer` has `capacity` writable bytes. Input and output must not overlap.
 */
PlayerResult player_lyrics_parse(const uint8_t *bytes,
                                 size_t length,
                                 uint8_t *buffer,
                                 size_t capacity,
                                 size_t *out_length);

#endif  /* AUVRA_PLAYER_H */
