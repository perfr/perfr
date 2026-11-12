export declare function sabAvailable(): boolean;
export declare function createByteRing(capacity: number): SharedArrayBuffer;
/** Returns false when the ring cannot fit `bytes` (caller should fall back). */
export declare function writeBytes(sab: SharedArrayBuffer, bytes: Uint8Array): boolean;
