// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.

// deno-lint-ignore-file no-var

/// <reference no-default-lib="true" />
/// <reference lib="esnext" />

/** @category Web APIs */
declare type ColorSpaceConversion = "default" | "none";

/** @category Web APIs */
declare type ImageOrientation = "flipY" | "from-image" | "none";

/** @category Web APIs */
declare type PremultiplyAlpha = "default" | "none" | "premultiply";

/** @category Web APIs */
declare type ResizeQuality = "high" | "low" | "medium" | "pixelated";

/** @category Web APIs */
declare type ImageBitmapSource = Blob | ImageData;

/** @category Web APIs */
declare interface ImageBitmapOptions {
  colorSpaceConversion?: ColorSpaceConversion;
  imageOrientation?: ImageOrientation;
  premultiplyAlpha?: PremultiplyAlpha;
  resizeHeight?: number;
  resizeQuality?: ResizeQuality;
  resizeWidth?: number;
}

/** @category Web APIs */
declare function createImageBitmap(
  image: ImageBitmapSource,
  options?: ImageBitmapOptions,
): Promise<ImageBitmap>;
/** @category Web APIs */
declare function createImageBitmap(
  image: ImageBitmapSource,
  sx: number,
  sy: number,
  sw: number,
  sh: number,
  options?: ImageBitmapOptions,
): Promise<ImageBitmap>;

/** @category Web APIs */
declare interface ImageBitmap {
  readonly height: number;
  readonly width: number;
  close(): void;
}

/** @category Web APIs */
declare var ImageBitmap: {
  prototype: ImageBitmap;
  new (): ImageBitmap;
};
