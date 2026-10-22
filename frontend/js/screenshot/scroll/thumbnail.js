//! 仅供预览的有界缩略采样；正式图像始终保留完整像素。
export const SCROLL_THUMBNAIL_WIDTH = 120;

export function createScrollThumbnail(image) {
    const width = Math.min(SCROLL_THUMBNAIL_WIDTH, image.width);
    const height = Math.max(1, Math.ceil(image.height * width / image.width));
    const result = new ImageData(width, height);
    const stepX = image.width / width, stepY = image.height / height;
    // 每个缩略像素最多 4×4 个均匀样本，工作量随缩略图而非原图像素增长。
    const nx = Math.min(4, Math.ceil(stepX)), ny = Math.min(4, Math.ceil(stepY));
    for (let y = 0; y < height; y++) for (let x = 0; x < width; x++) {
        let r = 0, g = 0, b = 0, a = 0;
        for (let sy = 0; sy < ny; sy++) for (let sx = 0; sx < nx; sx++) {
            const px = Math.min(image.width - 1, Math.floor((x + (sx + 0.5) / nx) * stepX));
            const py = Math.min(image.height - 1, Math.floor((y + (sy + 0.5) / ny) * stepY));
            const i = (py * image.width + px) * 4;
            const alpha = image.data[i + 3];
            r += image.data[i] * alpha; g += image.data[i + 1] * alpha; b += image.data[i + 2] * alpha;
            a += alpha;
        }
        const i = (y * width + x) * 4;
        if (a) { result.data[i] = r / a; result.data[i + 1] = g / a; result.data[i + 2] = b / a; }
        result.data[i + 3] = a / (nx * ny);
    }
    return result;
}
