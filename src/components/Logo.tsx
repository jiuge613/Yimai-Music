/**
 * Yimai 品牌标识：与桌面/任务栏/托盘图标同源（public/app-icon.png，
 * 即 src-tauri/icons/icon.ico 的 256px 图层来源 ym-icon-256.png）。
 * 三处图标共用同一资源，保证视觉一致；256px 源在 16–44px 显示尺寸下
 * 各 DPI 档位均直接命中原生像素，不失真。
 */
export default function Logo({ size = 44 }: { size?: number }) {
  return (
    <img
      src="/app-icon.png"
      alt="Yimai Music"
      width={size}
      height={size}
      draggable={false}
      className="shrink-0 select-none drop-shadow-[0_6px_16px_rgba(0,0,0,0.4)]"
    />
  );
}
