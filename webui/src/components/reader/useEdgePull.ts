import { useEffect, useRef, useState } from 'react'

/** raw pull distance that commits the jump to the adjacent book: the strip is fully slid
 *  in by ~70px of raw pull, so a longer threshold spends the rest of the gesture blind */
const PULL_THRESHOLD = 96
/** rubber-band ceiling: the visible offset compresses toward this, never past it */
const RUBBER_MAX = 240
/** wheel inertia must go quiet at the edge this long before wheeling may pull */
const WHEEL_ARM_QUIET_MS = 250
/** a wheel pull has no release, so the strip retracts once the wheel stops short of the threshold */
const WHEEL_RETRACT_MS = 600
/** hint strip height; the slide-in is capped at it */
const BAR_PX = 56

export type PullDirection = 'previous' | 'next'

/** rubber-band compression: 1:1 at the origin, decaying slope, bounded total */
const rubber = (raw: number) => (RUBBER_MAX * raw) / (RUBBER_MAX + raw * 0.9)

interface EdgePullOptions {
  scroller: () => HTMLDivElement | null
  bar: (dir: PullDirection) => HTMLDivElement | null
  /** false while pinch-zoomed or touch gestures are off: the edge keeps its plain behavior */
  enabled: boolean
  /** true once a pinch/pan owns the gesture */
  gestureConsumed: () => boolean
  /** a committed pull leaves the book directly (sibling or exit) */
  onLeave: (dir: PullDirection) => void
  /** changing books drops any in-flight pull */
  resetKey: unknown
}

interface Edge {
  raw: number
  wheelArmed: boolean
  firing: boolean
  timer: number | undefined
  retractTimer: number | undefined
}

const freshEdge = (): Edge => ({
  raw: 0,
  wheelArmed: false,
  firing: false,
  timer: undefined,
  retractTimer: undefined,
})

/**
 * Pull past the scroll edge to leave the book, for the continuous reader. At the top/bottom
 * edge, further input accumulates a raw pull distance; the hint strip follows it compressed
 * by the rubber() curve, and releasing the touch (or a wheel crossing) past the threshold
 * jumps straight to the adjacent book — the tap/keyboard path keeps its two-step toast.
 * Reversing shrinks the pull back; sliding all the way back cancels it.
 *
 * Wheel pulls only count after the wheel has gone quiet once at the edge, so fling inertia
 * is never read as an intentional pull. Touch pulls take the gesture over through a
 * non-passive touchmove once they start, so the native scroll cannot fight the strip.
 */
export function useEdgePull(options: EdgePullOptions) {
  const opts = useRef(options)
  useEffect(() => {
    opts.current = options
  })
  const [armed, setArmed] = useState<PullDirection | null>(null)
  const edges = useRef<Record<PullDirection, Edge>>({ previous: freshEdge(), next: freshEdge() })
  const suppressClick = useRef(false)
  useEffect(
    () => () => {
      for (const e of Object.values(edges.current)) {
        window.clearTimeout(e.timer)
        window.clearTimeout(e.retractTimer)
      }
    },
    [],
  )

  const write = (dir: PullDirection, offset: number, snapping: boolean) => {
    const el = opts.current.bar(dir)
    if (!el) return
    el.classList.toggle('snapping', snapping)
    const slide = Math.max(0, BAR_PX - Math.min(offset, BAR_PX))
    el.style.transform = `translateY(${dir === 'next' ? slide : -slide}px)`
  }

  const add = (dir: PullDirection, d: number) => {
    const e = edges.current[dir]
    if (e.firing) return
    const next = Math.max(0, e.raw + d)
    if (next === e.raw) return
    e.raw = next
    write(dir, rubber(e.raw), false)
    const want = e.raw >= PULL_THRESHOLD ? dir : null
    setArmed((prev) => (prev === want ? prev : want))
  }

  const fire = (dir: PullDirection) => {
    const e = edges.current[dir]
    if (e.firing) return
    e.firing = true
    window.clearTimeout(e.timer)
    e.timer = undefined
    window.clearTimeout(e.retractTimer)
    e.retractTimer = undefined
    suppressClick.current = true
    opts.current.onLeave(dir)
  }

  /** clears the gesture bookkeeping and the strip; the armed label settles with the next add() */
  const resetEdge = (dir: PullDirection, animate: boolean) => {
    const e = edges.current[dir]
    window.clearTimeout(e.timer)
    e.timer = undefined
    window.clearTimeout(e.retractTimer)
    e.retractTimer = undefined
    e.wheelArmed = false
    e.firing = false
    if (e.raw > 0) {
      e.raw = 0
      write(dir, 0, animate)
    }
  }

  const cancel = (dir: PullDirection, animate = true) => {
    resetEdge(dir, animate)
    setArmed((prev) => (prev === dir ? null : prev))
  }

  // changing books, or a zoom taking over the strip, drops any in-flight pull
  useEffect(() => {
    resetEdge('previous', false)
    resetEdge('next', false)
  }, [options.resetKey]) // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => {
    if (options.enabled) return
    resetEdge('previous', false)
    resetEdge('next', false)
  }, [options.enabled]) // eslint-disable-line react-hooks/exhaustive-deps

  /** after a wheel step, retract a pull the wheel left short; the wheel stays armed */
  const scheduleRetract = (dir: PullDirection) => {
    const e = edges.current[dir]
    window.clearTimeout(e.retractTimer)
    e.retractTimer = undefined
    if (e.firing || e.raw === 0) return
    e.retractTimer = window.setTimeout(() => {
      e.retractTimer = undefined
      if (e.firing || e.raw === 0) return
      e.raw = 0
      write(dir, 0, true)
      setArmed((prev) => (prev === dir ? null : prev))
    }, WHEEL_RETRACT_MS)
  }

  const feedWheelDir = (dir: PullDirection, d: number, atEdge: boolean) => {
    const e = edges.current[dir]
    if (e.firing) return
    if (!atEdge && e.raw === 0) {
      // still traveling toward the edge: this fling must go quiet there before it can pull
      e.wheelArmed = false
      window.clearTimeout(e.timer)
      e.timer = undefined
      return
    }
    window.clearTimeout(e.timer)
    e.timer = window.setTimeout(() => {
      e.timer = undefined
      e.wheelArmed = true
    }, WHEEL_ARM_QUIET_MS)
    // inertia from the fling that reached the edge is not a pull
    if (!e.wheelArmed) return
    add(dir, d)
    if (e.raw >= PULL_THRESHOLD) fire(dir)
    else scheduleRetract(dir)
  }

  const onWheel = (ev: React.WheelEvent<HTMLDivElement>) => {
    // ctrl+wheel is a trackpad pinch, not a pull
    if (!opts.current.enabled || ev.ctrlKey) return
    const el = opts.current.scroller()
    if (!el) return
    const normalize = (d: number) =>
      ev.deltaMode === 1 ? d * 16 : ev.deltaMode === 2 ? d * el.clientHeight : d
    const max = el.scrollHeight - el.clientHeight
    if (ev.deltaY > 0) {
      // a reverse wheel first shrinks an ongoing opposite pull instead of starting a new one
      if (edges.current.previous.raw > 0) {
        add('previous', -normalize(ev.deltaY))
        scheduleRetract('previous')
      } else feedWheelDir('next', normalize(ev.deltaY), el.scrollTop >= max - 1)
    } else if (ev.deltaY < 0) {
      if (edges.current.next.raw > 0) {
        add('next', normalize(ev.deltaY))
        scheduleRetract('next')
      } else feedWheelDir('previous', normalize(-ev.deltaY), el.scrollTop <= 1)
    }
  }

  // attaches to the scroller element once on mount, so the scroller must render
  // unconditionally: making it conditional or remounting it under a key would leave
  // the pull listening on a detached node, or on nothing
  useEffect(() => {
    const el = opts.current.scroller()
    if (!el) return
    let tracking = false
    let lastY = 0
    const atTop = () => el.scrollTop <= 1
    const atBottom = () => el.scrollTop >= el.scrollHeight - el.clientHeight - 1

    const onStart = (e: TouchEvent) => {
      // a touch that moved fires no click, so a flag left by the last pull must not eat this tap
      if (e.touches.length === 1) suppressClick.current = false
      tracking =
        e.touches.length === 1 && opts.current.enabled && !opts.current.gestureConsumed()
      if (tracking) lastY = e.touches[0].clientY
    }
    const onMove = (e: TouchEvent) => {
      if (!tracking) return
      if (e.touches.length !== 1 || opts.current.gestureConsumed()) {
        // a second finger turned this into a pinch: hand the gesture over and snap back
        tracking = false
        cancel('previous')
        cancel('next')
        return
      }
      const y = e.touches[0].clientY
      const d = lastY - y
      lastY = y
      const prevE = edges.current.previous
      const nextE = edges.current.next
      let own = false
      if (nextE.raw > 0 || (d > 0 && atBottom())) {
        if (d > 0 || nextE.raw > 0) {
          add('next', d)
          own = nextE.raw > 0
        }
      } else if (prevE.raw > 0 || (d < 0 && atTop())) {
        if (d < 0 || prevE.raw > 0) {
          add('previous', -d)
          own = prevE.raw > 0
        }
      }
      // the pull owns the gesture now; the native scroll must not fight the strip
      if (own) e.preventDefault()
    }
    const onEnd = () => {
      if (!tracking) return
      tracking = false
      if (edges.current.previous.raw > 0 || edges.current.next.raw > 0)
        suppressClick.current = true
      for (const dir of ['previous', 'next'] as const) {
        if (edges.current[dir].raw >= PULL_THRESHOLD) fire(dir)
        else cancel(dir)
      }
    }

    el.addEventListener('touchstart', onStart, { passive: true })
    el.addEventListener('touchmove', onMove, { passive: false })
    el.addEventListener('touchend', onEnd, { passive: true })
    el.addEventListener('touchcancel', onEnd, { passive: true })
    return () => {
      el.removeEventListener('touchstart', onStart)
      el.removeEventListener('touchmove', onMove)
      el.removeEventListener('touchend', onEnd)
      el.removeEventListener('touchcancel', onEnd)
    }
  }, []) // eslint-disable-line react-hooks/exhaustive-deps

  const consumeClick = () => {
    const v = suppressClick.current
    suppressClick.current = false
    return v
  }

  return { armed, onWheel, consumeClick }
}
