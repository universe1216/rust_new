// Copyright 2018 Ryan Dahl <ry@tinyclouds.org>
// All rights reserved. MIT License.
#ifndef HANDLERS_H_
#define HANDLERS_H_

#include <stdint.h>
#include "deno.h"

extern "C" {
void handle_code_fetch(Deno* d, uint32_t cmd_id, const char* module_specifier,
                       const char* containing_file);
}  // extern "C"
#endif  // HANDLERS_H_
