// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.
import { core, primordials } from "ext:core/mod.js";
const {
  op_console_size,
  op_isatty,
} = core.ensureFastOps();
const {
  Uint32Array,
} = primordials;

const size = new Uint32Array(2);

function consoleSize() {
  op_console_size(size);
  return { columns: size[0], rows: size[1] };
}

function isatty(rid) {
  return op_isatty(rid);
}

export { consoleSize, isatty };
