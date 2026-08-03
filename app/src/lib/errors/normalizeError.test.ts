import { describe, expect, it } from "vitest";
import { IpcValidationError } from "../ipc/validate";
import { normalizeError } from "./normalizeError";

describe("normalizeError", () => {

  it("extracts .message from an Error instance", () => {
    const result = normalizeError(new Error("디코딩 실패"));
    expect(result.message).toBe("디코딩 실패");
    expect(result.severity).toBe("error");
  });

  it("passes a plain string through unchanged (no paths)", () => {
    const result = normalizeError("IPC 연결 오류");
    expect(result.message).toBe("IPC 연결 오류");
    expect(result.severity).toBe("error");
  });

  it("returns '알 수 없는 오류' for null", () => {
    const result = normalizeError(null);
    expect(result.message).toBe("알 수 없는 오류");
    expect(result.severity).toBe("error");
  });

  it("returns '알 수 없는 오류' for undefined", () => {
    const result = normalizeError(undefined);
    expect(result.message).toBe("알 수 없는 오류");
    expect(result.severity).toBe("error");
  });

  it("converts an arbitrary object via String()", () => {
    const result = normalizeError({ code: 42 });
    expect(result.message).toBe("[object Object]");
    expect(result.severity).toBe("error");
  });

  it("handles a number input", () => {
    const result = normalizeError(404);
    expect(result.message).toBe("404");
    expect(result.severity).toBe("error");
  });


  it("recurses into .reason for a PromiseRejectionEvent", () => {
    const evt = new PromiseRejectionEvent("unhandledrejection", {
      promise: Promise.resolve(),
      reason: new Error("비동기 처리 오류"),
    });
    const result = normalizeError(evt);
    expect(result.message).toBe("비동기 처리 오류");
    expect(result.severity).toBe("error");
  });

  it("handles a PromiseRejectionEvent whose reason is a plain string", () => {
    const evt = new PromiseRejectionEvent("unhandledrejection", {
      promise: Promise.resolve(),
      reason: "네트워크 오류",
    });
    const result = normalizeError(evt);
    expect(result.message).toBe("네트워크 오류");
    expect(result.severity).toBe("error");
  });

  it("handles a PromiseRejectionEvent whose reason is null", () => {
    const evt = new PromiseRejectionEvent("unhandledrejection", {
      promise: Promise.resolve(),
      reason: null,
    });
    const result = normalizeError(evt);
    expect(result.message).toBe("알 수 없는 오류");
    expect(result.severity).toBe("error");
  });


  it("returns severity 'warning' and a Korean field message for IpcValidationError", () => {
    const err = new IpcValidationError("get_settings", "scan_folders", 42);
    const result = normalizeError(err);
    expect(result.severity).toBe("warning");
    expect(result.message).toContain("scan_folders");
    expect(result.message).toContain("IPC 응답 검증 오류");
  });

  it("does not expose the raw received value from IpcValidationError", () => {
    const sensitiveValue = { path: "C:\\Users\\user\\secret.mp4" };
    const err = new IpcValidationError("list_files", "path", sensitiveValue);
    const result = normalizeError(err);
    expect(result.message).not.toContain("C:\\Users\\user\\");
    expect(result.severity).toBe("warning");
  });


  it("strips Windows drive path prefix, leaving only the filename", () => {
    const result = normalizeError(
      new Error("파일 처리 실패: C:\\Users\\user\\videos\\clip.mp4"),
    );
    expect(result.message).not.toContain("C:\\Users\\user\\");
    expect(result.message).toContain("clip.mp4");
  });

  it("strips Windows forward-slash drive path", () => {
    const result = normalizeError("failed: C:/Users/user/videos/test.mp4");
    expect(result.message).not.toContain("C:/Users/user/");
    expect(result.message).toContain("test.mp4");
  });

  it("strips Windows UNC path prefix", () => {
    const result = normalizeError(
      new Error("오류: \\\\server\\share\\media\\video.mkv"),
    );
    expect(result.message).not.toContain("\\\\server\\share\\");
    expect(result.message).toContain("video.mkv");
  });

  it("strips POSIX /home/ prefix", () => {
    const result = normalizeError("오류: /home/user/videos/test.mp4");
    expect(result.message).not.toContain("/home/user/");
    expect(result.message).toContain("test.mp4");
  });

  it("strips POSIX /Users/ prefix", () => {
    const result = normalizeError(new Error("실패: /Users/user/Movies/film.mov"));
    expect(result.message).not.toContain("/Users/user/");
    expect(result.message).toContain("film.mov");
  });

  it("strips POSIX /mnt/ prefix", () => {
    const result = normalizeError("경로 오류: /mnt/data/archive/video.avi");
    expect(result.message).not.toContain("/mnt/data/archive/");
    expect(result.message).toContain("video.avi");
  });

  it("strips POSIX /var/ prefix", () => {
    const result = normalizeError("failed: /var/log/vidcull/daemon.log");
    expect(result.message).not.toContain("/var/log/");
    expect(result.message).toContain("daemon.log");
  });

  it("strips POSIX /tmp/ prefix", () => {
    const result = normalizeError(new Error("temp file: /tmp/vidcull-abc/clip.mp4"));
    expect(result.message).not.toContain("/tmp/vidcull-abc/");
    expect(result.message).toContain("clip.mp4");
  });

  it("strips POSIX /opt/ prefix", () => {
    const result = normalizeError("bin not found: /opt/ffmpeg/bin/ffmpeg");
    expect(result.message).not.toContain("/opt/ffmpeg/bin/");
    expect(result.message).toContain("ffmpeg");
  });

  it("strips POSIX /data/ prefix", () => {
    const result = normalizeError("index: /data/media/archive/source.mkv");
    expect(result.message).not.toContain("/data/media/");
    expect(result.message).toContain("source.mkv");
  });

  it("strips POSIX /srv/ prefix", () => {
    const result = normalizeError(new Error("serve: /srv/storage/clips/video.mp4"));
    expect(result.message).not.toContain("/srv/storage/");
    expect(result.message).toContain("video.mp4");
  });

  it("does not alter a message with no path", () => {
    const result = normalizeError(new Error("데몬과의 연결이 끊어졌습니다."));
    expect(result.message).toBe("데몬과의 연결이 끊어졌습니다.");
  });

  it("is a pure function — does not mutate the original Error", () => {
    const err = new Error("C:\\Users\\user\\video.mp4");
    const original = err.message;
    normalizeError(err);
    expect(err.message).toBe(original);
  });
});
