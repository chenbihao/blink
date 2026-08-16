//! 无依赖 PNG 解码器，仅供长截图离线 replay runner 读取开发模式导出的 RGBA/RGB PNG。

import {inflateSync} from 'node:zlib';

const PNG_SIGNATURE = Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]);

function paeth(a, b, c) {
    const p = a + b - c;
    const pa = Math.abs(p - a);
    const pb = Math.abs(p - b);
    const pc = Math.abs(p - c);
    return pa <= pb && pa <= pc ? a : (pb <= pc ? b : c);
}

export function decodeReplayPng(buffer) {
    if (!Buffer.isBuffer(buffer) || !buffer.subarray(0, 8).equals(PNG_SIGNATURE)) {
        throw new Error('不是有效 PNG');
    }
    let offset = 8;
    let width = 0;
    let height = 0;
    let bitDepth = 0;
    let colorType = 0;
    let interlace = 0;
    const idat = [];
    while (offset + 12 <= buffer.length) {
        const length = buffer.readUInt32BE(offset);
        const type = buffer.toString('ascii', offset + 4, offset + 8);
        const data = buffer.subarray(offset + 8, offset + 8 + length);
        if (type === 'IHDR') {
            width = data.readUInt32BE(0);
            height = data.readUInt32BE(4);
            bitDepth = data[8];
            colorType = data[9];
            interlace = data[12];
        } else if (type === 'IDAT') {
            idat.push(data);
        } else if (type === 'IEND') {
            break;
        }
        offset += length + 12;
    }
    if (width <= 0 || height <= 0 || bitDepth !== 8 || interlace !== 0
        || (colorType !== 6 && colorType !== 2)) {
        throw new Error(`不支持的 PNG 格式: ${width}x${height}, depth=${bitDepth}, color=${colorType}`);
    }
    const channels = colorType === 6 ? 4 : 3;
    const stride = width * channels;
    const inflated = inflateSync(Buffer.concat(idat));
    if (inflated.length !== (stride + 1) * height) throw new Error('PNG 扫描行长度不正确');
    const raw = Buffer.alloc(stride * height);
    for (let y = 0; y < height; y++) {
        const filter = inflated[y * (stride + 1)];
        const source = inflated.subarray(y * (stride + 1) + 1, (y + 1) * (stride + 1));
        const rowOffset = y * stride;
        for (let x = 0; x < stride; x++) {
            const left = x >= channels ? raw[rowOffset + x - channels] : 0;
            const up = y > 0 ? raw[rowOffset - stride + x] : 0;
            const upperLeft = y > 0 && x >= channels ? raw[rowOffset - stride + x - channels] : 0;
            let value;
            if (filter === 0) value = source[x];
            else if (filter === 1) value = source[x] + left;
            else if (filter === 2) value = source[x] + up;
            else if (filter === 3) value = source[x] + Math.floor((left + up) / 2);
            else if (filter === 4) value = source[x] + paeth(left, up, upperLeft);
            else throw new Error(`不支持的 PNG filter: ${filter}`);
            raw[rowOffset + x] = value & 255;
        }
    }
    const rgba = new Uint8ClampedArray(width * height * 4);
    for (let source = 0, target = 0; source < raw.length; source += channels, target += 4) {
        rgba[target] = raw[source];
        rgba[target + 1] = raw[source + 1];
        rgba[target + 2] = raw[source + 2];
        rgba[target + 3] = channels === 4 ? raw[source + 3] : 255;
    }
    return {width, height, data: rgba};
}
