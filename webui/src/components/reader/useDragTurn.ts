import { useEffect, useRef } from 'react'

/** travel before the gesture axis locks; below it the trailing click stays a tap */
const AXIS_LOCK_PX = 8
/** a release only counts as a tap when it ends near the press: a long path is still a tap if the finger came back */
const TAP_SLOP_PX = 10
/** a fast flick turns without the full travel, in followed and release-only swipes alike */
const FLICK_PX_PER_MS = 0.2
const FLICK_MIN_PX = 16
/** release velocity is the trailing window's endpoints: a finger slows before it lifts, so a drag-long average under-reads the flick */
const VELOCITY_WINDOW_MS = 100
/** a short swipe is a turn: the finger barely travels before a reader expects the page to move */
const COMMIT_FRACTION = 0.08
const COMMIT_MIN_PX = 32
/** caps the commit distance, so a wide window or a double-page spread never demands more than one honest drag */
const COMMIT_MAX_PX = 56
/** a gesture whose axis lock went the other way still turns when its slant proves it was a page turn */
const RESCUE_RATIO = 1.25
/** a crossfade turn shows this much of its progress under the finger */
const FADE_TRAVEL_PX = 160
/** how long an abandoned crossfade hint takes to ease back; the layer transitions for the same span */
const FADE_RELEASE_MS = 160
/** matches the track's turn transition, so a committed pull leaves after the slide-off settles */
const SETTLE_MS = 200

export type DragFadeDir = 'previous' | 'next'

/** the crossfade layer a release-only drag drives while the track itself never moves */
export interface DragFade {
  dir: DragFadeDir
  /** the slot that stays on top: fixed at the press, so landing the turn cannot re-point the fade */
  base: number
  phase: 'drag' | 'release'
}

interface DragTurnOptions {
  /** the track element whose transform the drag rewrites; frames go straight to style so a drag never re-renders */
  track: () => HTMLDivElement | null
  vertical: boolean
  flip: boolean
  /** false when the turn axis scrolls natively or pages crossfade: the swipe stays release-only */
  follow: boolean
  /** crossfade mode: a release-only swipe fades the old spread away instead of leaving the track still */
  fade: boolean
  /** whether the spread the fade reveals can already paint; until it can the drag only hints at it */
  canPaint: (dir: DragFadeDir) => boolean
  onFade: (fade: DragFade | null) => void
  animate: boolean
  /** the slot the track rests at: the spread index, or one past either end while a transition page shows */
  resting: number
  spreadCount: number
  /** a turn that only swaps a split wide page's half cannot be dragged, so that direction stays release-only */
  followForward: boolean
  followBackward: boolean
  swipe: boolean
  onNext: () => void
  onPrevious: () => void
  /** a committed pull at a boundary leaves the book directly, skipping the transition page's rest */
  onLeave: (dir: 'previous' | 'next') => void
  /** true once a pinch/pan owns the gesture */
  gestureConsumed: () => boolean
  /** swallow the click the browser fires after pointerup */
  markSwiped: () => void
}

type Status = 'idle' | 'pending' | 'drag' | 'release' | 'blocked' | 'settling'

/**
 * Drag-to-turn for the paged reader. The drag follows the finger one slot in either direction —
 * past the first/last spread the track's transition page is what gets revealed, and committing
 * that pull leaves the book straight away (the tap/wheel path keeps its two-step transition
 * page). Where the track cannot follow (native scroll on the turn axis, split-half turns) the
 * swipe stays release-only, and a crossfade instead fades the old spread away under the finger.
 */
export function useDragTurn(options: DragTurnOptions) {
  const opts = useRef(options)
  useEffect(() => {
    opts.current = options
  })
  const state = useRef({
    status: 'idle' as Status,
    x: 0,
    y: 0,
    size: 1,
    base: 0,
    delta: 0,
    velocity: 0,
    maxTravel: 0,
    trail: [] as { p: number; t: number }[],
    /** the overflow container the press landed in, and how far it had scrolled then */
    scroller: null as HTMLElement | null,
    scrollTop: 0,
    scrollLeft: 0,
    timer: undefined as number | undefined,
    fadeTimer: undefined as number | undefined,
  })
  const fadeRef = useRef<DragFade | null>(null)
  useEffect(
    () => () => {
      window.clearTimeout(state.current.timer)
      window.clearTimeout(state.current.fadeTimer)
      opts.current.track()?.style.removeProperty('--paged-fade')
    },
    [],
  )

  /** signed slot delta per drag pixel: a forward drag carries this sign along the turn axis */
  const forwardSign = () => {
    const o = opts.current
    return o.vertical ? -1 : o.flip ? 1 : -1
  }

  const write = (slot: number, px: number, settling: boolean) => {
    const el = opts.current.track()
    if (!el) return
    el.classList.toggle('dragging', !settling)
    // matches offset(): LTR/vertical rest at -slot*100%, RTL at +slot*100%; the drag delta is
    // already signed along the turn axis, so it composes with the base unflipped
    const pct = forwardSign() * slot * 100
    el.style.transform = `translate${opts.current.vertical ? 'Y' : 'X'}(calc(${pct}% + ${px}px))`
  }

  /** feeds the trailing velocity window; the release samples too, so a pause before lifting reads as no flick */
  const sample = (pos: number, t: number) => {
    const s = state.current
    s.trail.push({ p: pos, t })
    while (s.trail.length > 2 && t - s.trail[0].t > VELOCITY_WINDOW_MS) s.trail.shift()
    const oldest = s.trail[0]
    s.velocity = t > oldest.t ? (pos - oldest.p) / (t - oldest.t) : 0
  }

  /** measured when the finger lifts too: a rotation or a layout change mid-gesture leaves the press-time width stale */
  const commitThreshold = (size: number) => Math.max(COMMIT_MIN_PX, Math.min(COMMIT_MAX_PX, size * COMMIT_FRACTION))

  /** the outgoing spread reads its opacity from this variable, so a drag never re-renders per frame */
  const setFadeProgress = (p: number | null) => {
    const el = opts.current.track()
    if (!el) return
    if (p === null) el.style.removeProperty('--paged-fade')
    else el.style.setProperty('--paged-fade', String(p))
  }

  const reportFade = (next: DragFade | null) => {
    const cur = fadeRef.current
    if (cur === next) return
    if (cur && next && cur.dir === next.dir && cur.base === next.base && cur.phase === next.phase) return
    fadeRef.current = next
    opts.current.onFade(next)
  }

  const clearFade = () => {
    window.clearTimeout(state.current.fadeTimer)
    setFadeProgress(null)
    reportFade(null)
  }

  /** an uncommitted fade eases back instead of snapping, so a rejected pull reads as rejected */
  const releaseFade = () => {
    const s = state.current
    const cur = fadeRef.current
    if (!cur) return
    if (cur.phase === 'drag') reportFade({ ...cur, phase: 'release' })
    window.clearTimeout(s.fadeTimer)
    s.fadeTimer = window.setTimeout(() => {
      setFadeProgress(null)
      reportFade(null)
    }, FADE_RELEASE_MS)
  }

  const onPointerDown = (e: React.PointerEvent<HTMLDivElement>) => {
    const s = state.current
    const o = opts.current
    if (s.status === 'settling') return
    if (!o.swipe || !e.isPrimary || o.gestureConsumed()) return
    // a retracting hint from the last gesture must not bleed into this one
    clearFade()
    s.status = 'pending'
    s.x = e.clientX
    s.y = e.clientY
    s.size = o.vertical ? e.currentTarget.clientHeight : e.currentTarget.clientWidth
    s.base = o.resting
    s.delta = 0
    s.velocity = 0
    s.maxTravel = 0
    s.trail = [{ p: o.vertical ? e.clientY : e.clientX, t: e.timeStamp }]
    // the turn axis is locked to us, but the cross axis still scrolls: a release whose content
    // moved was a scroll, not a page turn trying to get out
    const scroller = (e.target as Element).closest<HTMLElement>('[data-reader-scroll]')
    s.scroller = scroller
    s.scrollTop = scroller?.scrollTop ?? 0
    s.scrollLeft = scroller?.scrollLeft ?? 0
  }

  const onPointerMove = (e: React.PointerEvent<HTMLDivElement>) => {
    const s = state.current
    if (s.status !== 'pending' && s.status !== 'drag' && s.status !== 'release' && s.status !== 'blocked') return
    if (!e.isPrimary) return
    const o = opts.current
    // a second finger turned this into a pinch: hand the gesture over and spring back
    if (o.gestureConsumed()) {
      if (s.status === 'drag') write(o.resting, 0, true)
      s.status = 'idle'
      releaseFade()
      return
    }
    const pos = o.vertical ? e.clientY : e.clientX
    const cross = (o.vertical ? e.clientX : e.clientY) - (o.vertical ? s.x : s.y)
    const d = pos - (o.vertical ? s.y : s.x)
    s.maxTravel = Math.max(s.maxTravel, Math.hypot(d, cross))
    if (s.status === 'pending') {
      if (Math.max(Math.abs(d), Math.abs(cross)) < AXIS_LOCK_PX) return
      // the cross axis stole the lock, but only the release can tell a scroll from a slanted turn
      if (Math.abs(cross) > Math.abs(d)) {
        s.status = 'blocked'
        return
      }
      const forward = Math.sign(d) === forwardSign()
      const room = forward ? Math.min(1, o.spreadCount - o.resting) : Math.min(1, o.resting + 1)
      s.status = o.follow && (forward ? o.followForward : o.followBackward) && room > 0 ? 'drag' : 'release'
    }
    sample(pos, e.timeStamp)
    if (s.status === 'drag') {
      const room = (fwd: boolean) => (fwd ? Math.min(1, o.spreadCount - o.resting) : Math.min(1, o.resting + 1))
      const slots = (d * forwardSign()) / s.size
      const clamped = Math.max(-room(false), Math.min(room(true), slots))
      const dc = clamped * s.size * forwardSign()
      s.delta = dc
      write(o.resting, dc, false)
      return
    }
    if (s.status !== 'release' || !o.fade) return
    // the fade only follows a direction the track could actually land on
    const dir: DragFadeDir = Math.sign(d) === forwardSign() ? 'next' : 'previous'
    const room = dir === 'next' ? Math.min(1, o.spreadCount - s.base) : Math.min(1, s.base + 1)
    if (room <= 0) {
      clearFade()
      return
    }
    const capacity = o.canPaint(dir) ? 1 : 0.25
    setFadeProgress(Math.min(Math.abs(d) / FADE_TRAVEL_PX, capacity))
    reportFade({ dir, base: s.base, phase: 'drag' })
  }

  const settle = (slot: number, action: (() => void) | null, delayAction: boolean) => {
    const s = state.current
    const o = opts.current
    s.status = 'settling'
    write(slot, 0, true)
    window.clearTimeout(s.timer)
    s.timer = window.setTimeout(
      () => {
        s.status = 'idle'
        action?.()
      },
      o.animate && delayAction ? SETTLE_MS : 0,
    )
  }

  const onPointerUp = (e: React.PointerEvent<HTMLDivElement>) => {
    const s = state.current
    const o = opts.current
    if (s.status !== 'pending' && s.status !== 'drag' && s.status !== 'release' && s.status !== 'blocked') return
    const pos = o.vertical ? e.clientY : e.clientX
    const cross = (o.vertical ? e.clientX : e.clientY) - (o.vertical ? s.x : s.y)
    const d = pos - (o.vertical ? s.y : s.x)
    s.maxTravel = Math.max(s.maxTravel, Math.hypot(d, cross))
    sample(pos, e.timeStamp)
    const size = o.vertical ? e.currentTarget.clientHeight : e.currentTarget.clientWidth
    const threshold = commitThreshold(size)
    const flick =
      Math.abs(s.velocity) >= FLICK_PX_PER_MS &&
      Math.abs(d) >= FLICK_MIN_PX &&
      Math.sign(s.velocity) === Math.sign(d)

    if (s.status !== 'drag') {
      const wasBlocked = s.status === 'blocked'
      s.status = 'idle'
      if (o.gestureConsumed()) {
        releaseFade()
        return
      }
      // the axis lock steers rendering, not the decision: a decisive turn still stands unless
      // the content scrolled under the finger, which makes it a real scroll after all
      const scrolled =
        s.scroller !== null &&
        (Math.abs(s.scroller.scrollTop - s.scrollTop) > 2 || Math.abs(s.scroller.scrollLeft - s.scrollLeft) > 2)
      const rescued =
        wasBlocked && !scrolled && Math.abs(d) >= threshold && Math.abs(d) >= RESCUE_RATIO * Math.abs(cross)
      const turns = (flick || Math.abs(d) >= threshold) && (rescued || Math.abs(d) >= Math.abs(cross))
      if (!turns) {
        // the click the browser fires where the finger lifts belongs to whatever is under it
        if (s.maxTravel > TAP_SLOP_PX) o.markSwiped()
        releaseFade()
        return
      }
      o.markSwiped()
      // the landing shows the target already at full opacity; the caller drops the layer when it lands
      if (fadeRef.current) setFadeProgress(1)
      if (Math.sign(d) === forwardSign()) o.onNext()
      else o.onPrevious()
      return
    }

    // a followed drag commits on the clamped travel: past a boundary the track has no room left
    // to move, and a split half swaps in place, so the commit distance is what the eye saw
    const turn = Math.abs(s.delta) >= threshold || flick ? s.delta : 0
    if (turn !== 0 || s.maxTravel > TAP_SLOP_PX) o.markSwiped()
    if (turn === 0) {
      settle(o.resting, null, false)
      return
    }
    const forward = Math.sign(turn) === forwardSign()
    // the commit can point away from the drag: past a transition page there is no slot to land on
    if ((forward ? o.spreadCount - o.resting : o.resting + 1) <= 0) {
      settle(o.resting, null, false)
      return
    }
    if (!(forward ? o.followForward : o.followBackward)) {
      settle(o.resting, forward ? o.onNext : o.onPrevious, false)
      return
    }
    const target = o.resting + (forward ? 1 : -1)
    // a pull landing past either end skips the transition page's rest and leaves the book
    if (target === -1 || target === o.spreadCount) {
      settle(target, () => o.onLeave(target === -1 ? 'previous' : 'next'), true)
    } else {
      settle(target, forward ? o.onNext : o.onPrevious, false)
    }
  }

  const onPointerCancel = () => {
    const s = state.current
    // the browser took the gesture for a scroll or a pinch: nothing to rescue, just retract the hint
    if (s.status === 'drag') settle(opts.current.resting, null, false)
    else s.status = 'idle'
    releaseFade()
  }

  return { onPointerDown, onPointerMove, onPointerUp, onPointerCancel }
}
