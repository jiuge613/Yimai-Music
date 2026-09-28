import { useCallback, useEffect, useMemo, useState } from "react";
import {
  Flame,
  Heart,
  Loader2,
  Music2,
  Play,
  Radio,
  Search,
  Settings2,
  TrendingUp,
  X,
} from "lucide-react";
import { api } from "../api";
import { useStore } from "../store";
import { fmtTime } from "../utils";
import type { LxSearchSong, LxSourceItem } from "../types";

/** 热门榜单入口：后端是关键词搜索接口，榜单名即搜索词（与酷我"榜单选歌"一致的交互） */
const CHARTS: { label: string; keyword: string }[] = [
  { label: "热歌榜", keyword: "热歌" },
  { label: "新歌榜", keyword: "新歌" },
  { label: "飙升榜", keyword: "飙升" },
  { label: "流行榜", keyword: "流行" },
  { label: "经典榜", keyword: "经典老歌" },
  { label: "民谣榜", keyword: "民谣" },
  { label: "古风榜", keyword: "古风" },
  { label: "抖音榜", keyword: "抖音热歌" },
];

export default function RankingView() {
  const toast = useStore((s) => s.toast);
  const setView = useStore((s) => s.setView);
  const toggleLikeOnline = useStore((s) => s.toggleLikeOnline);
  const savedOnline = useStore((s) => s.savedOnline);
  // 搜索结果跨模块保留：切走再回来不丢（store 里网易云/QQ/酷狗都是这么做的）
  const rankingSearch = useStore((s) => s.rankingSearch);
  const setRankingSearch = useStore((s) => s.setRankingSearch);

  const [sources, setSources] = useState<LxSourceItem[]>([]);
  const [sourceId, setSourceId] = useState<number>(0); // 0 = 自动（第一个启用的音源）
  const [platform, setPlatform] = useState(""); // "" = 自动

  const [kw, setKw] = useState(rankingSearch?.kw ?? "");
  const [query, setQuery] = useState(rankingSearch?.kw ?? "");
  const [loading, setLoading] = useState(false);
  const [searched, setSearched] = useState(rankingSearch?.searched ?? false);
  const [emptyKw, setEmptyKw] = useState(false);
  const [error, setError] = useState("");
  const [songs, setSongs] = useState<LxSearchSong[]>(rankingSearch?.songs ?? []);
  const [via, setVia] = useState(rankingSearch?.via ?? "");
  const [sourceError, setSourceError] = useState(rankingSearch?.sourceError ?? "");
  const [resultPlatform, setResultPlatform] = useState(rankingSearch?.resultPlatform ?? "");
  const [resultSourceId, setResultSourceId] = useState<number | null>(
    rankingSearch?.resultSourceId ?? null,
  );
  const [playingId, setPlayingId] = useState<string>("");

  // 已启用的音源（排行榜的搜索与取链都依赖它；没有也能用内置平台兜底）
  useEffect(() => {
    let alive = true;
    api
      .lxListSources()
      .then((all) => {
        if (!alive) return;
        setSources(all.filter((s) => s.enabled));
      })
      .catch(() => {
        /* 读取失败按"未接入音源"处理，仍可走内置平台 */
      });
    return () => {
      alive = false;
    };
  }, []);

  // 当前选中音源声明的平台（供筛选 chips）
  const platformOptions = useMemo(() => {
    const picked = sources.find((s) => s.id === sourceId) ?? sources[0];
    return picked?.platforms ?? [];
  }, [sources, sourceId]);

  const submit = useCallback(
    async (word?: string) => {
      const text = (word ?? kw).trim();
      if (!text) {
        // 空关键词：不发起请求，给出即时提示
        setEmptyKw(true);
        setSongs([]);
        setError("");
        setSearched(false);
        setRankingSearch(null);
        return;
      }
      setEmptyKw(false);
      setKw(text);
      setQuery(text);
      setLoading(true);
      setError("");
      setSearched(true);
      try {
        const r = await api.lxSearch({
          sourceId: sourceId || null,
          platform,
          keyword: text,
          limit: 30,
        });
        setSongs(r.songs ?? []);
        setVia(r.via ?? "");
        setSourceError(r.sourceError ?? "");
        setResultPlatform(r.platform ?? "");
        setResultSourceId(r.sourceId ?? null);
        // 写回 store：切到其它模块再回来时恢复同一份结果
        setRankingSearch({
          kw: text,
          songs: r.songs ?? [],
          via: r.via ?? "",
          sourceError: r.sourceError ?? "",
          resultPlatform: r.platform ?? "",
          resultSourceId: r.sourceId ?? null,
          searched: true,
        });
      } catch (e) {
        setSongs([]);
        setError(String(e));
        setRankingSearch({
          kw: text,
          songs: [],
          via: "",
          sourceError: "",
          resultPlatform: "",
          resultSourceId: null,
          searched: true,
        });
      } finally {
        setLoading(false);
      }
    },
    [kw, platform, sourceId, setRankingSearch]
  );

  const playSong = useCallback(
    async (s: LxSearchSong) => {
      const sid = resultSourceId ?? (sourceId || sources[0]?.id || 0);
      setPlayingId(s.id);
      try {
        await api.lxPlaySong({
          sourceId: s.viaGd ? 0 : sid,
          platform: s.platform || resultPlatform || platform || "kg",
          songId: s.id,
          title: s.title,
          artist: s.artist ?? "",
          album: s.album ?? "",
          durationMs: s.durationMs ?? 0,
          extra: s.extra ?? "",
          viaGd: s.viaGd ?? false,
          picId: s.picId ?? "",
        });
      } catch (e) {
        toast(`播放失败：${e}`, "error");
      } finally {
        setPlayingId("");
      }
    },
      [platform, resultPlatform, resultSourceId, sourceId, sources, toast]
  );

  /** 收藏到“我喜欢”：LX 曲目的 rid 形如 "sourceId:platform:songId"，
   *  取链上下文（酷狗 hash / QQ media_mid）放进 mediaMid，供二次播放复用 */
  const likeSong = useCallback(
    (s: LxSearchSong) => {
      const sid = resultSourceId ?? (sourceId || sources[0]?.id || 0);
      const plat = s.platform || resultPlatform || platform || "kg";
      const rid = `${sid}:${plat}:${s.id}`;
      void toggleLikeOnline({
        kind: "lx",
        id: rid,
        name: s.title,
        artist: s.artist ?? "",
        album: s.album ?? "",
        cover: "",
        durationMs: s.durationMs ?? 0,
        mediaMid: s.extra ?? "",
        vip: false,
      });
    },
    [platform, resultPlatform, resultSourceId, sourceId, sources, toggleLikeOnline]
  );

  const activeChart = CHARTS.find((c) => c.keyword === query)?.label ?? "";
  const hasSource = sources.length > 0;

  return (
    <div className="flex-1 min-h-0 flex flex-col">
      <header className="px-8 pt-7 pb-4">
        <div className="flex items-end justify-between gap-5">
          <div className="min-w-0 anim-rise shrink-0">
            <div className="flex items-center gap-2 text-[11px] text-[var(--ink-3)] tracking-[0.24em] mb-2">
              <TrendingUp size={13} />
              排行榜
            </div>
            <h1 className="text-[30px] font-extrabold leading-none tracking-tight text-[var(--ink)]">
              大家都在听
            </h1>
            <div className="flex items-center gap-3 mt-3 text-[12.5px] text-[var(--ink-2)]">
              {searched && !loading && (
                <span className="tabular-nums">{songs.length} 首歌曲</span>
              )}
              {activeChart && (
                <span className="chip" style={{ background: "var(--shade)" }}>
                  <Flame size={12} />
                  {activeChart}
                </span>
              )}
            </div>
          </div>

          {/* 搜索框（页面顶部，回车即搜） */}
          <div className="flex items-center gap-2.5 flex-1 max-w-[520px] mb-1">
            <div className="relative flex-1">
              <Search
                size={14}
                className="absolute left-3.5 top-1/2 -translate-y-1/2 text-[var(--ink-3)]"
              />
              <input
                type="text"
                value={kw}
                onChange={(e) => {
                  setKw(e.target.value);
                  if (emptyKw) setEmptyKw(false);
                }}
                onKeyDown={(e) => e.key === "Enter" && submit()}
                placeholder="搜索歌曲、歌手或专辑"
                className="w-full h-10 rounded-xl bg-[var(--shade)] border border-[var(--line)] pl-9 pr-8 text-[12.5px] text-[var(--ink)] placeholder:text-[var(--ink-3)] focus:border-[rgba(240,162,74,0.45)] transition-colors"
              />
              {kw && (
                <button
                  className="absolute right-2.5 top-1/2 -translate-y-1/2 text-[var(--ink-3)] hover:text-[var(--ink)]"
                  onClick={() => {
                    setKw("");
                    setSongs([]);
                    setQuery("");
                    setSearched(false);
                    setError("");
                    setEmptyKw(false);
                  }}
                  title="清空"
                >
                  <X size={14} />
                </button>
              )}
            </div>
            <button
              className="btn-primary h-10"
              onClick={() => submit()}
              disabled={loading}
            >
              {loading ? (
                <Loader2 size={13} className="animate-spin" />
              ) : (
                <Search size={13} />
              )}
              搜索
            </button>
          </div>

          {/* 音源选择 */}
          <div className="shrink-0 flex flex-col items-end gap-2 mb-1 min-w-[190px]">
            <div className="flex items-center gap-2">
              <Radio size={13} className="text-[var(--ink-3)]" />
              <select
                value={sourceId}
                onChange={(e) => setSourceId(Number(e.target.value))}
                className="h-8 rounded-lg bg-[var(--shade)] border border-[var(--line)] px-2 text-[12px] text-[var(--ink)] outline-none max-w-[190px]"
                title="选择用于搜索与取链的音源"
              >
                <option value={0}>
                  {hasSource ? "自动选择音源" : "未接入音源（内置平台）"}
                </option>
                {sources.map((s) => (
                  <option key={s.id} value={s.id}>
                    {s.name}
                  </option>
                ))}
              </select>
            </div>
            {!hasSource && (
              <button
                className="btn-secondary !py-1 !px-2.5 text-[11.5px]"
                onClick={() => setView("settings")}
              >
                <Settings2 size={12} />
                去设置添加音源
              </button>
            )}
          </div>
        </div>

        {/* 榜单快捷入口 + 平台筛选 */}
        <div className="flex items-center gap-2 mt-4 flex-wrap">
          {CHARTS.map((c) => (
            <button
              key={c.label}
              onClick={() => submit(c.keyword)}
              className={`chip transition-colors ${
                activeChart === c.label ? "text-[var(--accent-strong)]" : ""
              }`}
              style={{
                background:
                  activeChart === c.label ? "var(--accent-weak)" : "var(--shade)",
              }}
            >
              {c.label}
            </button>
          ))}
          {platformOptions.length > 0 && (
            <div className="flex items-center gap-1.5 ml-1 pl-3 border-l border-[var(--line)]">
              <button
                onClick={() => setPlatform("")}
                className="chip"
                style={{
                  background: platform === "" ? "var(--accent-weak)" : "var(--shade)",
                  color: platform === "" ? "var(--accent-strong)" : undefined,
                }}
              >
                全部平台
              </button>
              {platformOptions.map((p) => (
                <button
                  key={p.code}
                  onClick={() => setPlatform(p.code)}
                  className="chip"
                  style={{
                    background:
                      platform === p.code ? "var(--accent-weak)" : "var(--shade)",
                    color: platform === p.code ? "var(--accent-strong)" : undefined,
                  }}
                  title={`平台代码 ${p.code}`}
                >
                  {p.name || p.code}
                </button>
              ))}
            </div>
          )}
        </div>

        {/* 数据来源提示 */}
        {(via || sourceError) && !loading && (
          <div className="flex items-center gap-2 mt-3 text-[11.5px] text-[var(--ink-3)]">
            <span className="chip" style={{ background: "var(--shade)" }}>
              <Music2 size={12} />
              {via}
            </span>
            {sourceError && (
              <span className="truncate" title={sourceError}>
                音源搜索不可用，已回退内置平台：{sourceError}
              </span>
            )}
          </div>
        )}

        {emptyKw && (
          <div className="mt-3 text-[12px] text-[var(--accent-strong)]">
            请先输入歌曲名、歌手或专辑关键词再搜索
          </div>
        )}
      </header>

      {/* 结果区 */}
      <div className="flex-1 min-h-0 flex flex-col px-6 pb-[86px]">
        <div className="glass rounded-3xl flex-1 min-h-0 flex flex-col overflow-hidden">
          {songs.length > 0 && (
            <div className="grid grid-cols-[52px_minmax(180px,1fr)_minmax(110px,220px)_minmax(110px,240px)_72px_44px] items-center gap-4 h-10 px-5 border-b border-[var(--line)] text-[10.5px] text-[var(--ink-3)] tracking-[0.18em]">
              <span className="text-center">排名</span>
              <span>歌曲</span>
              <span>歌手</span>
              <span>专辑</span>
              <span className="text-right">时长</span>
              <span />
            </div>
          )}

          <div className="flex-1 min-h-0 overflow-y-auto px-2 py-1.5">
            {/* 加载中 */}
            {loading && (
              <div className="h-full flex flex-col items-center justify-center gap-3 text-[var(--ink-2)]">
                <Loader2 size={22} className="animate-spin text-[var(--accent)]" />
                <div className="text-[12.5px]">正在搜索「{kw}」…</div>
              </div>
            )}

            {/* 错误 */}
            {!loading && error && (
              <div className="h-full flex flex-col items-center justify-center gap-3 px-8 text-center">
                <div className="text-[13.5px] text-[var(--ink)]">搜索失败</div>
                <div className="text-[12px] text-[var(--ink-2)] leading-relaxed max-w-[520px]">
                  {error}
                </div>
                <button className="btn-secondary !py-1.5 !px-3" onClick={() => submit()}>
                  重试
                </button>
              </div>
            )}

            {/* 无结果 */}
            {!loading && !error && searched && songs.length === 0 && (
              <div className="h-full flex flex-col items-center justify-center gap-3 text-[var(--ink-3)]">
                <div
                  className="w-16 h-16 rounded-2xl flex items-center justify-center"
                  style={{
                    background: "rgba(255,255,255,0.04)",
                    border: "1px solid rgba(255,255,255,0.06)",
                  }}
                >
                  <Search size={26} />
                </div>
                <div className="text-[13.5px]">
                  没有找到与「{query}」相关的歌曲
                </div>
                <div className="text-[12px] text-[var(--ink-3)]">
                  换个关键词，或点击上方榜单试试
                </div>
              </div>
            )}

            {/* 未搜索：引导 */}
            {!loading && !error && !searched && songs.length === 0 && (
              <div className="h-full flex flex-col items-center justify-center gap-3 text-[var(--ink-3)]">
                <div
                  className="w-16 h-16 rounded-2xl flex items-center justify-center"
                  style={{
                    background: "rgba(255,255,255,0.04)",
                    border: "1px solid rgba(255,255,255,0.06)",
                  }}
                >
                  <TrendingUp size={26} />
                </div>
                <div className="text-[13.5px]">
                  {hasSource
                    ? "输入关键词开始搜索，或点击上方榜单"
                    : "尚未接入音源，搜索将使用内置平台"}
                </div>
                <div className="text-[12px]">
                  {hasSource
                    ? "搜索与播放都会走你已配置的音源"
                    : "在设置 → 音源管理 添加音源后，可直接用音源搜索与取链"}
                </div>
              </div>
            )}

            {/* 结果列表 */}
            {!loading && songs.length > 0 && (
              <div className="flex flex-col">
                {songs.map((s, i) => {
                  const playing = playingId === s.id;
                  const top = i < 3;
                  // “我喜欢”收藏态：与 likeSong 的 rid 规则一致
                  const sid = resultSourceId ?? (sourceId || sources[0]?.id || 0);
                  const plat = s.platform || resultPlatform || platform || "kg";
                  const likeKey = `lx-${sid}:${plat}:${s.id}`;
                  const liked = !!savedOnline[likeKey];
                  return (
                    <div
                      key={`${s.platform}-${s.id}-${i}`}
                      onDoubleClick={() => playSong(s)}
                      className="group grid grid-cols-[52px_minmax(180px,1fr)_minmax(110px,220px)_minmax(110px,240px)_72px_44px_32px] items-center gap-3 h-[52px] px-3 rounded-xl hover:bg-[var(--shade)] transition-colors"
                    >
                      <span
                        className="text-center text-[13px] tabular-nums font-semibold"
                        style={{
                          color: top ? "var(--accent-strong)" : "var(--ink-3)",
                        }}
                      >
                        {i + 1}
                      </span>
                      <div className="min-w-0">
                        <div className="text-[13px] text-[var(--ink)] truncate">
                          {s.title}
                        </div>
                      </div>
                      <div className="text-[12.5px] text-[var(--ink-2)] truncate">
                        {s.artist || "未知歌手"}
                      </div>
                      <div className="text-[12.5px] text-[var(--ink-2)] truncate">
                        {s.album || "未知专辑"}
                      </div>
                      <span className="text-right text-[12px] text-[var(--ink-3)] tabular-nums">
                        {s.durationMs ? fmtTime(s.durationMs) : "--:--"}
                      </span>
                      <button
                        className="w-8 h-8 rounded-full flex items-center justify-center shrink-0 transition-transform hover:scale-105"
                        style={{
                          background: "linear-gradient(135deg, #ffc470, #e8823f)",
                        }}
                        title="播放"
                        onClick={() => playSong(s)}
                        disabled={playing}
                      >
                        {playing ? (
                          <Loader2 size={13} className="animate-spin text-[var(--ink)]" />
                        ) : (
                          <Play size={13} className="fill-current text-[var(--ink)] ml-px" />
                        )}
                      </button>
                      <button
                        className="w-8 h-8 rounded-full flex items-center justify-center shrink-0 opacity-0 group-hover:opacity-100 transition-all hover:scale-105"
                        style={{ background: "var(--shade)" }}
                        title={liked ? "取消喜欢" : "加入“我喜欢”"}
                        onClick={() => likeSong(s)}
                      >
                        <Heart
                          size={15}
                          className={
                            liked
                              ? "fill-[#e0533f] text-[#e0533f]"
                              : "text-[var(--ink-2)]"
                          }
                        />
                      </button>
                    </div>
                  );
                })}
              </div>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
