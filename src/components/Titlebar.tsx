import { getCurrentWindow } from "@tauri-apps/api/window";
import { Minus, Square, X, Copy, ChevronLeft } from "lucide-react";
import { useEffect, useState } from "react";
import { useStore } from "../store";

export default function Titlebar() {
  const [maximized, setMaximized] = useState(false);
  const win = getCurrentWindow();
  const canGoBack =
    useStore((s) => s.navHistory.length > 0) ||
    (useStore((s) => s.view) === "detail" && useStore((s) => s.detailStack.length) > 0);
  const back = useStore((s) => s.back);

  useEffect(() => {
    let disposed = false;
    let un: (() => void) | undefined;
    const p = win.onResized(async () => setMaximized(await win.isMaximized()));
    p.then((u) => {
      // 组件可能已在 promise resolve 前卸载（StrictMode 双挂载），避免泄漏监听器
      if (disposed) u();
      else un = u;
    });
    return () => {
      disposed = true;
      un?.();
    };
  }, []);

  return (
    <>
      {/* 顶部拖拽热区：扩大到 36px 高，向下压在内容区顶部的留白（各视图 pt-7）上。
          绝对定位不参与布局，因此内容组件位置完全不变；data-tauri-drag-region
          使空白处可拖动窗口。 */}
      <div
        data-tauri-drag-region
        className="absolute top-0 left-0 right-0 h-9 z-[54] pointer-events-auto"
      />

      {/* 返回上一层：有返回栈时才出现（逐层返回在线曲库 → 详情 → …） */}
      {canGoBack && (
        <button
          className="titlebar-btn absolute top-0 left-0 w-9 h-9 z-[56] flex items-center justify-center text-[var(--ink-2)] hover:bg-[var(--shade)] hover:text-[var(--ink)] transition-colors"
          onClick={() => back()}
          title="返回上一层（Alt + ←）"
        >
          <ChevronLeft size={16} />
        </button>
      )}

      {/* 窗口控制按钮（右上角悬浮，压在热区之上、内容区右上角留白处） */}
      <div className="absolute top-0 right-0 flex items-center h-9 z-[56]">
        <button
          className="titlebar-btn w-11 h-9 flex items-center justify-center text-[var(--ink-2)] hover:bg-[var(--shade)] hover:text-[var(--ink)] transition-colors"
          onClick={() => win.minimize()}
        >
          <Minus size={14} />
        </button>
        <button
          className="titlebar-btn w-11 h-9 flex items-center justify-center text-[var(--ink-2)] hover:bg-[var(--shade)] hover:text-[var(--ink)] transition-colors"
          onClick={() => win.toggleMaximize()}
        >
          {maximized ? <Copy size={12} className="-scale-x-100" /> : <Square size={11} />}
        </button>
        <button
          className="titlebar-btn w-11 h-9 flex items-center justify-center text-[var(--ink-2)] hover:bg-[#c73e2e] hover:text-[var(--ink)] transition-colors"
          onClick={() => win.close()}
        >
          <X size={15} />
        </button>
      </div>
    </>
  );
}
