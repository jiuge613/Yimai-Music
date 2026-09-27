import { useMemo } from "react";
import {
  AlertCircle,
  CheckCircle2,
  Clock3,
  Download,
  FileDown,
  FolderOpen,
  ListMusic,
  Play,
  RefreshCw,
  RotateCw,
  Settings,
  Trash2,
} from "lucide-react";
import { useStore } from "../store";
import CoverImg from "../components/CoverImg";
import type { DownloadTask } from "../types";

const fmtSize = (b: number) => {
  if (!b || b < 0) return "—";
  if (b < 1024) return `${b} B`;
  if (b < 1024 * 1024) return `${(b / 1024).toFixed(0)} KB`;
  return `${(b / 1024 / 1024).toFixed(1)} MB`;
};

const fmtTime = (ts: number) => {
  if (!ts) return "—";
  const d = new Date(ts * 1000);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
};

const platformName = (k: string) =>
  k === "netease" ? "网易云" : k === "qq" ? "QQ音乐" : k === "kugou" ? "酷狗" : k === "lx" ? "音源" : k;

/** 下载管理：已下载 / 下载中 两个页签，行内可重试、定位、删除。
 *  队列下载由后端 worker 执行，这里只负责展示与操作。 */
export default function DownloadView() {
  const tasks = useStore((s) => s.downloadTasks);
  const tab = useStore((s) => s.downloadTab);
  const setTab = useStore((s) => s.setDownloadTab);
  const refresh = useStore((s) => s.refreshDownloads);
  const retry = useStore((s) => s.retryDownload);
  const del = useStore((s) => s.deleteDownload);
  const clear = useStore((s) => s.clearDownloads);
  const openLoc = useStore((s) => s.openDownloadLocation);
  const exportCsv = useStore((s) => s.exportDownloads);
  const tracks = useStore((s) => s.tracks);
  const playTracks = useStore((s) => s.playTracks);

  const done = useMemo(() => tasks.filter((t) => t.status === "done"), [tasks]);
  const active = useMemo(
    () => tasks.filter((t) => t.status !== "done"),
    [tasks]
  );
  const rows = tab === "done" ? done : active;
  const activeCount = active.length;

  /** 播放全部已下载：把任务映射成本地曲目后交给统一播放入口。
   *  匹配不上（文件被移走/删除）时给个提示，别静默什么都没发生。 */
  const playAll = () => {
    const local = done
      .map((t) => tracks.find((x) => x.path === t.filePath))
      .filter((x): x is NonNullable<typeof x> => !!x);
    if (!local.length) {
      useStore
        .getState()
        .toast("已下载的歌曲未出现在本地音乐（可能已被移走），请先扫描本地音乐", "error");
      return;
    }
    playTracks(local, 0);
  };

  return (
    <div className="flex-1 min-h-0 flex flex-col">
      <header className="px-8 pt-7 pb-4">
        <div className="flex items-end justify-between gap-5 flex-wrap">
          <div className="min-w-0">
            <h1 className="text-[30px] font-extrabold leading-none tracking-tight text-[var(--ink)]">
              下载管理
            </h1>
            <div className="flex items-center gap-3 mt-3 text-[12.5px] text-[var(--ink-2)]">
              <span className="tabular-nums">已下载 {done.length}</span>
              <span className="w-1 h-1 rounded-full bg-[var(--ink-3)]" />
              <span className="tabular-nums">
                {activeCount > 0 ? `下载中 ${activeCount}` : "暂无进行中的任务"}
              </span>
            </div>
          </div>

          {/* 批量操作 */}
          <div className="flex items-center gap-2 shrink-0">
            <button className="btn-secondary" onClick={playAll} disabled={!done.length}>
              <Play size={14} className="fill-current" />
              播放全部
            </button>
            <button
              className="btn-secondary"
              onClick={() => clear("all", false)}
              disabled={!tasks.length}
            >
              <Trash2 size={14} />
              全部删除
            </button>
            <button
              className="btn-secondary"
              onClick={() => exportCsv(tab)}
              disabled={!rows.length}
              title="导出当前页签为 CSV（落到下载目录）"
            >
              <FileDown size={14} />
              导出歌曲
            </button>
            <button
              className="btn-ghost w-8 h-8"
              onClick={() => useStore.getState().setView("settings")}
              title="下载设置（保存目录 / 音质）"
            >
              <Settings size={15} />
            </button>
            <button className="btn-ghost w-8 h-8" onClick={() => void refresh()} title="刷新">
              <RefreshCw size={15} />
            </button>
          </div>
        </div>

        {/* 页签：已下载 / 下载中 */}
        <div className="flex items-center gap-2 mt-4">
          {(
            [
              { key: "done" as const, label: "已下载", n: done.length },
              { key: "active" as const, label: "下载中", n: activeCount },
            ]
          ).map((t) => (
            <button
              key={t.key}
              onClick={() => setTab(t.key)}
              className={`px-3.5 h-8 rounded-full text-[12.5px] transition-colors ${
                tab === t.key
                  ? "bg-[var(--accent)] text-[var(--accent-on)] font-semibold"
                  : "text-[var(--ink-2)] hover:bg-[var(--shade-hover)]"
              }`}
            >
              {t.label}
              <span className="ml-1.5 tabular-nums opacity-80">{t.n}</span>
            </button>
          ))}
        </div>
      </header>

      {/* 列表 */}
      <div className="flex-1 min-h-0 flex flex-col px-6 pb-[86px]">
        <div className="glass rounded-3xl flex-1 min-h-0 flex flex-col overflow-hidden">
          {rows.length > 0 && (
            <div className="grid grid-cols-[56px_minmax(200px,1fr)_minmax(140px,200px)_minmax(140px,240px)_100px_150px_96px] items-center gap-4 h-10 px-5 border-b border-[var(--line)] text-[10.5px] text-[var(--ink-3)] tracking-[0.18em]">
              <span className="text-center">序号</span>
              <span>歌曲</span>
              <span>歌手</span>
              <span>专辑</span>
              <span className="text-right">大小</span>
              <span className="text-right">完成时间</span>
              <span className="text-right">操作</span>
            </div>
          )}

          <div className="flex-1 min-h-0 overflow-y-auto">
            {rows.length === 0 ? (
              <div className="flex flex-col items-center justify-center gap-3 pt-20 text-[var(--ink-3)]">
                {tab === "done" ? (
                  <>
                    <CheckCircle2 size={26} className="opacity-45" />
                    <div className="text-[13.5px]">
                      {tasks.length ? "还没有下载完成的歌曲" : "下载记录为空"}
                    </div>
                    <div className="text-[11.5px]">
                      在曲库或在线曲库里右键「下载到本地」，任务就会出现在这里
                    </div>
                  </>
                ) : (
                  <>
                    <Download size={26} className="opacity-45" />
                    <div className="text-[13.5px]">当前没有下载中的任务</div>
                  </>
                )}
              </div>
            ) : (
              rows.map((t, i) => <Row key={t.id} t={t} idx={i} onRetry={retry} onOpen={openLoc} onDel={del} />)
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

function Row({
  t,
  idx,
  onRetry,
  onOpen,
  onDel,
}: {
  t: DownloadTask;
  idx: number;
  onRetry: (id: string) => Promise<void>;
  onOpen: (id: string) => Promise<void>;
  onDel: (id: string, delFile: boolean) => Promise<void>;
}) {
  const pct = t.size > 0 ? Math.min(100, Math.round((t.received / t.size) * 100)) : 0;
  const failed = t.status === "failed";
  const queued = t.status === "queued";
  return (
    <div
      className={`group grid grid-cols-[56px_minmax(200px,1fr)_minmax(140px,200px)_minmax(140px,240px)_100px_150px_96px] items-center gap-4 h-[64px] px-4 rounded-2xl transition-colors ${
        failed ? "opacity-70" : "hover:bg-[var(--shade-hover)]"
      }`}
      title={failed ? t.error : undefined}
    >
      <span className="text-center text-[12.5px] tabular-nums text-[var(--ink-3)]">
        {String(idx + 1).padStart(2, "0")}
      </span>

      {/* 歌曲：封面 + 歌名 + 下载中进度条 / 失败原因 */}
      <div className="flex items-center gap-3 min-w-0">
        <CoverImg
          src={t.cover}
          seed={t.title}
          className="w-10 h-10 rounded-lg shadow-[var(--cover-shadow-sm)] shrink-0"
          iconSize={15}
        />
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2 min-w-0">
            <span className="text-[13px] text-[var(--ink)] truncate">{t.title}</span>
            <span className="text-[9.5px] px-1.5 py-0.5 rounded bg-[var(--shade-strong)] text-[var(--ink-3)] font-medium shrink-0">
              {platformName(t.kind)}
            </span>
            {failed && (
              <span className="inline-flex items-center gap-1 text-[10.5px] text-[#e0533f] shrink-0">
                <AlertCircle size={11} />
                失败
              </span>
            )}
            {queued && (
              <span className="inline-flex items-center gap-1 text-[10.5px] text-[var(--ink-3)] shrink-0">
                <Clock3 size={11} />
                排队中
              </span>
            )}
            {t.status === "downloading" && (
              <span className="text-[10.5px] text-[var(--accent-strong)] tabular-nums shrink-0">
                {pct}%
              </span>
            )}
          </div>
          {t.status === "downloading" && (
            <div className="h-[3px] rounded-full bg-[var(--shade-strong)] mt-1.5 overflow-hidden">
              <div
                className="h-full rounded-full transition-[width] duration-300"
                style={{ width: `${pct}%`, background: "var(--accent)" }}
              />
            </div>
          )}
          {failed && t.error && (
            <div className="text-[11px] text-[#e0533f]/85 truncate mt-0.5">{t.error}</div>
          )}
        </div>
      </div>

      <span className="text-[12.5px] text-[var(--ink-2)] truncate">{t.artist || "—"}</span>
      <span className="text-[12.5px] text-[var(--ink-3)] truncate">{t.album || "—"}</span>
      <span className="text-right text-[12.5px] text-[var(--ink-2)] tabular-nums">
        {t.size > 0 ? fmtSize(t.size) : "—"}
      </span>
      <span className="text-right text-[12px] text-[var(--ink-3)] tabular-nums">
        {t.finishedAt ? fmtTime(t.finishedAt) : "—"}
      </span>

      {/* 操作 */}
      <div className="flex items-center justify-end gap-1 pr-1">
        {failed && (
          <button className="btn-ghost w-8 h-8" onClick={() => void onRetry(t.id)} title="重试">
            <RotateCw size={14} />
          </button>
        )}
        {t.status === "done" && (
          <>
            <button
              className="btn-ghost w-8 h-8"
              onClick={() => void onOpen(t.id)}
              title="打开所在位置"
            >
              <FolderOpen size={14} />
            </button>
            <button
              className="btn-ghost w-8 h-8"
              onClick={() => void onDel(t.id, true)}
              title="删除记录并删除文件"
            >
              <Trash2 size={14} className="hover:text-[#e0533f]" />
            </button>
          </>
        )}
        {queued && (
          <button
            className="btn-ghost w-8 h-8"
            onClick={() => void onDel(t.id, false)}
            title="从队列移除"
          >
            <Trash2 size={14} className="hover:text-[#e0533f]" />
          </button>
        )}
        {t.status === "downloading" && (
          <span className="px-2 text-[10.5px] text-[var(--ink-3)]">下载中</span>
        )}
      </div>
    </div>
  );
}
