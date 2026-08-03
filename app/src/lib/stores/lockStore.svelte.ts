
const LS_KEY = "vidcull.screenLock";

interface PersistedConfig {
  enabled: boolean;
  saltB64: string;
  hashB64: string;
  idleMinutes: number;
  lockOnBlur: boolean;
}


function bytesToBase64(bytes: Uint8Array): string {
  return btoa(String.fromCharCode(...bytes));
}

function base64ToBytes(b64: string): Uint8Array {
  return Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
}

function isSSR(): boolean {
  return typeof localStorage === "undefined";
}

function hasCrypto(): boolean {
  return typeof crypto !== "undefined" && !!crypto?.subtle;
}


let isLocked = $state(false);

let enabled = $state(false);
let saltB64 = $state("");
let hashB64 = $state("");
let idleMinutes = $state(0);
let lockOnBlur = $state(false);


function persistConfig(): void {
  if (isSSR()) return;
  const cfg: PersistedConfig = {
    enabled,
    saltB64,
    hashB64,
    idleMinutes,
    lockOnBlur,
  };
  localStorage.setItem(LS_KEY, JSON.stringify(cfg));
}


export const lockStore = {

  get isLocked(): boolean {
    return isLocked;
  },


  get enabled(): boolean {
    return enabled;
  },

  get idleMinutes(): number {
    return idleMinutes;
  },

  get lockOnBlur(): boolean {
    return lockOnBlur;
  },


  get hasPassword(): boolean {
    return hashB64.length > 0;
  },



  loadConfig(): void {
    if (isSSR()) return;
    const raw = localStorage.getItem(LS_KEY);
    if (!raw) return;
    try {
      const cfg = JSON.parse(raw) as Partial<PersistedConfig>;
      enabled = cfg.enabled ?? false;
      saltB64 = cfg.saltB64 ?? "";
      hashB64 = cfg.hashB64 ?? "";
      idleMinutes = cfg.idleMinutes ?? 0;
      lockOnBlur = cfg.lockOnBlur ?? false;
    } catch {
    }
  },



  async setPassword(pw: string): Promise<void> {
    if (!hasCrypto()) return;
    const salt = crypto.getRandomValues(new Uint8Array(16));
    const pwBytes = new TextEncoder().encode(pw);
    const combined = new Uint8Array(salt.length + pwBytes.length);
    combined.set(salt);
    combined.set(pwBytes, salt.length);
    const hashBuf = await crypto.subtle.digest("SHA-256", combined);
    saltB64 = bytesToBase64(salt);
    hashB64 = bytesToBase64(new Uint8Array(hashBuf));
    enabled = true;
    persistConfig();
  },


  async verify(pw: string): Promise<boolean> {
    if (!hasCrypto() || !saltB64 || !hashB64) return false;
    const salt = base64ToBytes(saltB64);
    const pwBytes = new TextEncoder().encode(pw);
    const combined = new Uint8Array(salt.length + pwBytes.length);
    combined.set(salt);
    combined.set(pwBytes, salt.length);
    const hashBuf = await crypto.subtle.digest("SHA-256", combined);
    const computed = new Uint8Array(hashBuf);
    const stored = base64ToBytes(hashB64);
    if (computed.length !== stored.length) return false;
    let diff = 0;
    for (let i = 0; i < computed.length; i++) {
      diff |= computed[i] ^ stored[i];
    }
    return diff === 0;
  },


  disable(): void {
    hashB64 = "";
    saltB64 = "";
    enabled = false;
    isLocked = false;
    persistConfig();
  },



  lock(): void {
    if (enabled) isLocked = true;
  },


  unlock(): void {
    isLocked = false;
  },


  setIdleMinutes(n: number): void {
    idleMinutes = n;
    persistConfig();
  },

  setLockOnBlur(b: boolean): void {
    lockOnBlur = b;
    persistConfig();
  },
};
