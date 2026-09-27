import {
  ChevronDown,
  Clock3,
  Cloud,
  Disc3,
  Globe,
  Heart,
  LayoutGrid,
  Library,
  ListMusic,
  Loader2,
  Music2,
  Palette,
  Pencil,
  Plus,
  Radio,
  Settings,
  DiscAlbum,
  Download as DownloadIcon,
  TrendingUp,
} from "lucide-react";
import { useState } from "react";
import { createPortal } from "react-dom";
import { useStore } from "../store";
import Logo from "./Logo";
import SkinPicker from "./SkinPicker";
import { InputModal } from "./Dialogs";
import CoverImg from "./CoverImg";
import { clampMenuPos } from "../utils";
import { useDragList } from "../hooks/useDragList";
import type { ViewName } from "../types";

// 侧栏一级：常用入口，常驻显示。
// 「排行榜」按参考设计留在顶层（不归入「更多」分组）。
const NAV_MAIN: { key: ViewName; label: string; icon: typeof Library }[] = [
  { key: "library", label: "本地音乐", icon: Library },
  { key: "liked", label: "我喜欢", icon: Heart },
  { key: "recent", label: "最近播放", icon: Clock3 },
  { key: "ranking", label: "排行榜", icon: TrendingUp },
];

// 「更多」分组内的平铺入口
const NAV_MORE: { key: ViewName; label: string; icon: typeof Library }[] = [
  { key: "downloads", label: "下载管理", icon: DownloadIcon },
  { key: "sources", label: "在线音源", icon: Radio },
];

// 嵌套在「更多」下的「知名平台」子分组
const NAV_PLATFORMS: { key: ViewName; label: string; icon: typeof Library }[] =
  [
    { key: "netease", label: "网易云", icon: Cloud },
    { key: "qq", label: "QQ音乐", icon: DiscAlbum },
    { key: "kugou", label: "酷狗", icon: Music2 },
  ];

export default function Sidebar() {
  const view = useStore((s) => s.view);
  const viewParam = useStore((s) => s.viewParam);
  const setView = useStore((s) => s.setView);
  const playlists = useStore((s) => s.playlists);
  const scan = useStore((s) => s.scan);
  const tracks = useStore((s) => s.tracks);
  const likedOnline = useStore((s) => s.likedOnline);
  const reorderPlaylists = useStore((s) => s.reorderPlaylists);
  const renamePlaylist = useStore((s) => s.renamePlaylist);
  const [plModalOpen, setPlModalOpen] = useState(false);
  // 播放列表右键菜单 / 重命名弹窗
  const [menu, setMenu] = useState<{ x: number; y: number; id: number } | null>(
    null
  );
  const [renaming, setRenaming] = useState<{ id: number; name: string } | null>(
    null
  );
  const [skinOpen, setSkinOpen] = useState(false);
  // 「更多」折叠组展开状态（默认展开，保证平铺入口与知名平台都可见）
  const [moreOpen, setMoreOpen] = useState(
    () => localStorage.getItem("yimai.moreOpen") !== "0"
  );
  const toggleMore = () =>
    setMoreOpen((v) => {
      localStorage.setItem("yimai.moreOpen", v ? "0" : "1");
      return !v;
    });
  // 「知名平台」子分组展开状态（记忆到 localStorage，默认展开）
  const [platOpen, setPlatOpen] = useState(
    () => localStorage.getItem("yimai.platformsOpen") !== "0"
  );
  const togglePlat = () =>
    setPlatOpen((v) => {
      localStorage.setItem("yimai.platformsOpen", v ? "0" : "1");
      return !v;
    });
  // 组内任一入口处于激活态（折叠时用于在组标题上保留高亮提示）
  const moreActive = [...NAV_MORE, ...NAV_PLATFORMS].some((p) => p.key === view);
  const platformActive = NAV_PLATFORMS.some((p) => p.key === view);
  // 长按拖动排序：行序列 = 本地 state 的播放列表顺序
  const { rowProps } = useDragList((from, to) => {
    const ids = playlists.map((p) => p.id);
    const [moved] = ids.splice(from, 1);
    ids.splice(to, 0, moved);
    void reorderPlaylists(ids);
  });

  // 计数 = 本地喜欢 + 在线喜欢（与“我喜欢”视图显示的内容一致）
  const likedCount = tracks.filter((t) => t.liked).length + likedOnline.length;
  const totalDuration = tracks.reduce((acc, t) => acc + t.duration, 0);
  const hours = Math.floor(totalDuration / 3600);
  const mins = Math.floor((totalDuration % 3600) / 60);

  return (
    <aside className="w-[236px] shrink-0 flex flex-col px-4 pt-3 relative z-20">
      {/* 品牌区（与桌面图标同构的黑胶 logo） */}
      <div className="flex items-center gap-3.5 px-3 pt-2 pb-6">
        <Logo size={44} />
        <div className="min-w-0">
          <div className="text-[15px] font-bold tracking-wide leading-tight text-[var(--ink)]">
            Yimai Music
          </div>
          <div className="text-[10px] text-[var(--ink-3)] tracking-[0.22em] mt-0.5">
            HI-FI PLAYER
          </div>
        </div>
      </div>

      {/* 导航 + 播放列表（可滚动区） */}
      <div className="flex-1 min-h-0 overflow-y-auto overflow-x-hidden pb-3">
        <nav className="flex flex-col gap-1.5">
          {NAV_MAIN.map(({ key, label, icon: Icon }) => {
            const active = view === key;
            return (
              <button
                key={key}
                onClick={() => setView(key)}
                className={`nav-item relative h-11 pl-4 pr-3 rounded-xl flex items-center gap-3.5 text-[13.5px] transition-all duration-200 ${
                  active
                    ? "nav-active font-semibold"
                    : "text-[var(--ink-2)]"
                }`}
                style={{ border: "1px solid transparent" }}
              >
                {active && (
                  <span
                    className="nav-bar absolute left-0 top-1/2 -translate-y-1/2 w-[3px] h-5 rounded-full"
                  />
                )}
                <Icon size={17} strokeWidth={1.9} />
                {label}
                {key === "liked" && likedCount > 0 && (
                  <span className="nav-count ml-auto text-[11.5px] text-[var(--ink-3)] tabular-nums">
                    {likedCount}
                  </span>
                )}
              </button>
            );
          })}

          {/* ── 「更多」折叠组：在线音源 / 排行榜 + 嵌套的知名平台 ── */}
          <div className="my-1.5 h-px bg-[var(--line)]" />

          <button
            onClick={toggleMore}
            aria-expanded={moreOpen}
            className={`nav-item relative h-11 pl-4 pr-3 rounded-xl flex items-center gap-3.5 text-[13.5px] transition-all duration-200 ${
              moreActive
                ? "nav-active font-semibold"
                : "text-[var(--ink-2)]"
            }`}
            style={{ border: "1px solid transparent" }}
          >
            {moreActive && (
              <span className="nav-bar absolute left-0 top-1/2 -translate-y-1/2 w-[3px] h-5 rounded-full" />
            )}
            <LayoutGrid size={17} strokeWidth={1.9} />
            更多
            <ChevronDown
              size={15}
              className={`ml-auto text-[var(--ink-3)] transition-transform duration-200 ${
                moreOpen ? "" : "-rotate-90"
              }`}
            />
          </button>

          {moreOpen && (
            <>
              {/* 更多 · 平铺入口 */}
              {NAV_MORE.map(({ key, label, icon: Icon }) => {
                const active = view === key;
                return (
                  <button
                    key={key}
                    onClick={() => setView(key)}
                    className={`nav-item relative h-10 pl-[46px] pr-3 rounded-xl flex items-center gap-3 text-[13px] transition-all duration-200 ${
                      active
                        ? "nav-active font-semibold"
                        : "text-[var(--ink-2)]"
                    }`}
                    style={{ border: "1px solid transparent" }}
                  >
                    {active && (
                      <span className="nav-bar absolute left-0 top-1/2 -translate-y-1/2 w-[3px] h-5 rounded-full" />
                    )}
                    <Icon size={15} strokeWidth={1.9} className={active ? "" : "text-[var(--ink-3)]"} />
                    {label}
                  </button>
                );
              })}

              {/* 更多 · 「知名平台」子分组 */}
              <button
                onClick={togglePlat}
                aria-expanded={platOpen}
                className={`nav-item relative h-10 pl-[46px] pr-3 rounded-xl flex items-center gap-3 text-[13px] transition-all duration-200 ${
                  platformActive
                    ? "nav-active font-semibold"
                    : "text-[var(--ink-2)]"
                }`}
                style={{ border: "1px solid transparent" }}
              >
                {platformActive && (
                  <span className="nav-bar absolute left-0 top-1/2 -translate-y-1/2 w-[3px] h-5 rounded-full" />
                )}
                <Globe
                  size={15}
                  strokeWidth={1.9}
                  className={platformActive ? "" : "text-[var(--ink-3)]"}
                />
                知名平台
                <ChevronDown
                  size={14}
                  className={`ml-auto text-[var(--ink-3)] transition-transform duration-200 ${
                    platOpen ? "" : "-rotate-90"
                  }`}
                />
              </button>

              {platOpen &&
                NAV_PLATFORMS.map(({ key, label, icon: Icon }) => {
                  const active = view === key;
                  return (
                    <button
                      key={key}
                      onClick={() => setView(key)}
                      className={`nav-item relative h-10 pl-[72px] pr-3 rounded-xl flex items-center gap-3 text-[12.5px] transition-all duration-200 ${
                        active
                          ? "nav-active font-semibold"
                          : "text-[var(--ink-2)]"
                      }`}
                      style={{ border: "1px solid transparent" }}
                    >
                      {active && (
                        <span className="nav-bar absolute left-0 top-1/2 -translate-y-1/2 w-[3px] h-5 rounded-full" />
                      )}
                      <Icon
                        size={14}
                        strokeWidth={1.9}
                        className={active ? "" : "text-[var(--ink-3)]"}
                      />
                      {label}
                    </button>
                  );
                })}
            </>
          )}
        </nav>

        {/* 播放列表 */}
        <div className="mt-7 mb-2 px-4 flex items-center justify-between">
          <span className="text-[10.5px] font-semibold text-[var(--ink-3)] tracking-[0.18em]">
            播放列表
          </span>
          <button
            className="btn-ghost w-7 h-7 rounded-lg"
            title="新建播放列表"
            onClick={() => setPlModalOpen(true)}
          >
            <Plus size={15} />
          </button>
        </div>

        <div className="flex flex-col gap-1">
          {playlists.map((p, i) => {
            const active = view === "playlist" && viewParam === p.id;
            const originTip =
              p.originName && p.originName !== p.name
                ? `原名：${p.originName}（长按可拖动排序）`
                : "长按可拖动排序";
            return (
              <div key={p.id} {...rowProps(i)}>
                {/* 整行原是 button：会被 useDragList 的“行内控件不触发拖拽”
                    保护拦掉，故行容器用 div，点击行为由内层承接 */}
                <div
                  role="button"
                  tabIndex={-1}
                  title={originTip}
                  onClick={() => setView("playlist", p.id)}
                  onContextMenu={(ev) => {
                    ev.preventDefault();
                    setMenu({ x: ev.clientX, y: ev.clientY, id: p.id });
                  }}
                  className={`nav-item h-10 pl-4 pr-3 rounded-xl flex items-center gap-3 text-[13px] transition-all cursor-pointer ${
                    active ? "nav-active" : "text-[var(--ink-2)]"
                  }`}
                >
                  <ListMusic size={15} className={active ? "" : "text-[var(--ink-3)]"} />
                  <span className="truncate">{p.name}</span>
                  <span className="nav-count ml-auto text-[11.5px] text-[var(--ink-3)] tabular-nums">
                    {p.entries.length}
                  </span>
                </div>
              </div>
            );
          })}
          {!playlists.length && (
            <button
              onClick={() => setPlModalOpen(true)}
              className="mx-1 h-10 rounded-xl border border-dashed border-[rgba(243,233,216,0.14)] text-[12px] text-[var(--ink-3)] hover:text-[var(--ink-2)] hover:border-[rgba(243,233,216,0.26)] transition-colors"
            >
              + 创建播放列表
            </button>
          )}
        </div>
      </div>

      {/* 底部固定区：设置按钮中心与播放条垂直中心对齐（播放条中心距底 36px） */}
      <div className="shrink-0 flex flex-col justify-end pb-[10px] gap-2">
        {scan.active && (
          <div className="flex items-center gap-2 px-4 text-[12px] text-[var(--accent)]">
            <Loader2 size={13} className="animate-spin" />
            <span className="truncate">
              扫描中 {scan.total ? `${scan.done}/${scan.total}` : "…"}
            </span>
          </div>
        )}

        {/* 设置 + 皮肤：同一行，皮肤在设置右侧 */}
        <div className="flex gap-2">
          <button
            onClick={() => setView("settings")}
            className={`nav-item flex-1 min-w-0 h-11 px-4 rounded-xl flex items-center gap-3.5 text-[13.5px] transition-all ${
              view === "settings" ? "nav-active" : "text-[var(--ink-2)]"
            }`}
          >
            <Settings size={17} strokeWidth={1.9} />
            设置
          </button>
          <button
            onClick={() => setSkinOpen(true)}
            title="皮肤"
            className={`nav-item h-11 px-3.5 rounded-xl flex items-center gap-2 text-[13.5px] shrink-0 transition-all ${
              skinOpen ? "nav-active" : "text-[var(--ink-2)]"
            }`}
          >
            <Palette size={17} strokeWidth={1.9} />
            皮肤
          </button>
        </div>
      </div>

      <SkinPicker open={skinOpen} onClose={() => setSkinOpen(false)} />

      <InputModal
        open={plModalOpen}
        title="新建播放列表"
        placeholder="播放列表名称"
        confirmText="创建"
        onClose={() => setPlModalOpen(false)}
        onConfirm={(name) => useStore.getState().createPlaylist(name)}
      />

      <InputModal
        open={renaming != null}
        title="重命名播放列表"
        initialValue={renaming?.name}
        confirmText="重命名"
        onClose={() => setRenaming(null)}
        onConfirm={(name) => {
          if (renaming) void renamePlaylist(renaming.id, name);
        }}
      />

      {/* 播放列表右键菜单（Portal 到 body：fixed 才相对视口） */}
      {menu &&
        createPortal(
          <div
            className="fixed z-[75] w-[150px] glass-strong rounded-xl p-1.5 shadow-2xl anim-menu"
            style={(() => {
              const p = clampMenuPos(menu.x, menu.y, 150, 80);
              return { left: p.x, top: p.y };
            })()}
            onMouseDown={(ev) => ev.stopPropagation()}
            onMouseLeave={() => setMenu(null)}
          >
            <button
              className="w-full h-8 px-2.5 rounded-lg flex items-center gap-2.5 text-[12.5px] text-[var(--ink)] hover:bg-[var(--shade-strong)] text-left"
              onClick={() => {
                const pl = playlists.find((p) => p.id === menu.id);
                if (pl) setRenaming({ id: pl.id, name: pl.name });
                setMenu(null);
              }}
            >
              <Pencil size={13} className="text-[var(--ink-3)]" />
              重命名
            </button>
          </div>,
          document.body
        )}
    </aside>
  );
}
