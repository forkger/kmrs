import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
} from 'react'
import { useReducedMotion } from 'motion/react'
import { useTranslation } from 'react-i18next'
import type { BookDto, ReadingDirection } from '@/lib/api/types'
import { useReaderSettings, type ScaleType } from '@/lib/store/readerSettings'
import { useEffectivePageFilter, useEffectiveSettings } from '@/components/reader/effectiveSettings'
import { buildSpreads, isLandscape, measureWhenKnown, slotAspectRatio, type SpreadPage } from '@/lib/utils/spreads'
import { cn } from '@/lib/utils/cn'
import { BookTransition } from './BookTransition'
import { useWindowKeys } from './keys'
import { PageImage } from './PageImage'
import { tapAction } from './tapZones'
import { useDragTurn, type DragFade, type DragFadeDir } from './useDragTurn'
import { usePinchZoom } from './usePinchZoom'

interface PagedReaderProps {
  pages: SpreadPage[]
  page: number
  direction: ReadingDirection
  onPageChange: (page: number) => void
  onToggleChrome: () => void
  /** the book the transition pages name, plus its neighbours; null past either end of the list */
  book: BookDto
  previousBook: BookDto | null
  nextBook: BookDto | null
  onLeaveBook: (direction: 'previous' | 'next') => void
  onMeasure: (page: number, width: number, height: number) => void
  /** shown half of a split wide page; owned by the caller so progress saving can see it */
  splitHalf: 0 | 1
  onSplitHalfChange: (half: 0 | 1) => void
}

/** The page a spread reports back: the visually last one (right page in LTR, left page in RTL). */
function spreadCurrentPage(spread: SpreadPage[]): number {
  return spread.length === 2 && !spread[1].blank ? spread[1].number : spread[0].number
}

/**
 * Slot geometry lives on the wrapper div now (PageImage centers its spinner on it), so every
 * mode needs one definite axis for the aspect-ratio fallback to resolve against: WIDTH modes
 * take it from the width classes, the rest anchor on viewport height or measured pixels.
 */
function slotClass(scale: ScaleType, double: boolean, measured: boolean): string {
  switch (scale) {
    case 'WIDTH':
      return cn('self-start', double ? 'w-1/2' : 'w-full')
    case 'WIDTH_SHRINK_ONLY':
      return cn('self-start', measured ? (double ? 'max-w-1/2' : 'max-w-full') : double ? 'w-1/2' : 'w-full')
    case 'HEIGHT':
      return 'h-dvh'
    case 'ORIGINAL':
      return measured ? 'shrink-0' : 'h-dvh'
    default:
      return cn(!measured && 'h-dvh', double ? 'max-w-1/2' : 'max-w-full')
  }
}

function slotStyle(scale: ScaleType, double: boolean, stretchSmall: boolean, p: SpreadPage): CSSProperties {
  const measured = p.width && p.height
  if (scale === 'ORIGINAL' && measured) return { width: p.width, height: p.height }
  if (scale === 'WIDTH_SHRINK_ONLY' && measured)
    return { width: `min(${double ? '50%' : '100%'}, ${p.width}px)`, aspectRatio: slotAspectRatio(p) }
  // capped at the natural height so a page smaller than the viewport is not upscaled unless asked
  if (scale === 'SCREEN' && measured)
    return { height: stretchSmall ? '100dvh' : `min(100dvh, ${p.height}px)`, aspectRatio: slotAspectRatio(p) }
  return { aspectRatio: slotAspectRatio(p) }
}

/**
 * A split box shows one half of a wide page: one axis follows the scale mode and
 * aspect-ratio derives the other, so the box behaves like a standalone half-page image.
 */
function splitBoxStyle(scale: ScaleType, width: number, height: number): CSSProperties {
  const style: CSSProperties = { aspectRatio: `${width / 2} / ${height}` }
  switch (scale) {
    case 'WIDTH':
      style.width = '100%'
      break
    case 'WIDTH_SHRINK_ONLY':
      style.width = `min(100%, ${width / 2}px)`
      break
    case 'HEIGHT':
      style.height = '100dvh'
      break
    case 'ORIGINAL':
      style.width = width / 2
      break
    default:
      style.width = `min(calc(100dvh * ${width / 2 / height}), 100%)`
  }
  return style
}

/** How long a turn keeps the old page up while the target loads; past this the spinner is the better answer */
const TURN_HOLD_MS = 250

/**
 * The crossfade layer a drag drives: the outgoing spread reads its opacity from the variable the
 * gesture writes per frame, and eases back when the pull is abandoned.
 */
function fadeLayerStyle(fade: DragFade): CSSProperties {
  return fade.phase === 'drag'
    ? { opacity: 'calc(1 - var(--paged-fade, 0))', transition: 'none' }
    : { opacity: 1, transition: 'opacity 160ms ease-out' }
}

export function PagedReader({
  pages,
  page,
  direction,
  book,
  previousBook,
  nextBook,
  onPageChange,
  onToggleChrome,
  onLeaveBook,
  onMeasure,
  splitHalf,
  onSplitHalfChange,
}: PagedReaderProps) {
  const { scale, pageLayout, splitWide } = useEffectiveSettings()
  const stretchSmall = useReaderSettings((s) => s.stretchSmall)
  const animations = useReaderSettings((s) => s.animations)
  const pageTransition = useReaderSettings((s) => s.pageTransition)
  const tapZones = useReaderSettings((s) => s.tapZones)
  const swipe = useReaderSettings((s) => s.swipe)
  // blank slots bypass PageImage, so they pick the page filter up here
  const pageFilter = useEffectivePageFilter()
  const reduceMotion = useReducedMotion()
  const { t } = useTranslation('reader')

  const spreads = useMemo(() => buildSpreads(pages, pageLayout), [pages, pageLayout])
  const pageToSpread = useMemo(() => {
    const map: number[] = []
    spreads.forEach((spread, i) => spread.forEach((p) => !p.blank && (map[p.number] = i)))
    return map
  }, [spreads])

  const flip = direction === 'RIGHT_TO_LEFT'
  const vertical = direction === 'VERTICAL'
  const spreadIndex = pageToSpread[page] ?? 0
  // a measured re-pair can change the visible spread's composition while its index stays
  const spreadKey = spreads[spreadIndex]?.map((p) => (p.blank ? 'b' : p.number)).join('-') ?? ''
  // in single-page layout a wide page turns in two halves instead of one spread step
  const splitOn = splitWide && pageLayout === 'SINGLE_PAGE'
  const curSpread = spreads[spreadIndex]
  const atFirst = spreadIndex === 0
  const atLast = spreadIndex === spreads.length - 1
  // the transition page shown past an end of the book; any move away from that end drops it
  const [transition, setTransition] = useState<'previous' | 'next' | null>(null)
  if ((transition === 'previous' && !atFirst) || (transition === 'next' && !atLast)) setTransition(null)
  const curSplittable =
    splitOn && curSpread?.length === 1 && !curSpread[0].blank && isLandscape(curSpread[0])
  // revealing a spread that is still loading or decoding drops to the bare background, and a
  // run of those turns strobes, so the old spread stays up until the target can paint. Split
  // pages opt out: the outgoing spread loses its crop the moment the turn starts
  const [readyPages, setReadyPages] = useState<ReadonlySet<string>>(() => new Set())
  const markReady = useCallback(
    (src: string) => setReadyPages((prev) => (prev.has(src) ? prev : new Set(prev).add(src))),
    [],
  )
  const [shown, setShown] = useState({ spreads, index: spreadIndex })
  const targetReady = curSpread?.every((p) => p.blank || readyPages.has(p.url)) ?? true
  if (shown.spreads !== spreads || (shown.index !== spreadIndex && (splitOn || targetReady)))
    setShown({ spreads, index: spreadIndex })
  const shownIndex = shown.index
  useEffect(() => {
    if (shownIndex === spreadIndex) return
    const timer = window.setTimeout(() => setShown({ spreads, index: spreadIndex }), TURN_HOLD_MS)
    return () => window.clearTimeout(timer)
  }, [shownIndex, spreadIndex, spreads])
  const animate = animations && !reduceMotion
  // a page narrower than the viewport leaves a gap that a slide sweeps through the reading
  // area as a dark flash on every turn. Split pages keep sliding: leaving the current spread
  // drops the crop, which a fade would show in place
  const crossfade = animate && !splitOn && pageTransition === 'FADE'
  // fit modes stay inside the viewport; the rest need a scrollable slide
  const scrollable = scale === 'WIDTH' || scale === 'WIDTH_SHRINK_ONLY' || scale === 'ORIGINAL'
  // hugging the content keeps horizontal overflow reachable by scroll while small content stays centered
  const hugContent = scale === 'ORIGINAL' || scale === 'HEIGHT'

  // the fade layer is the outgoing/target pair of a crossfade drag, held in React only so the
  // two spreads swap stacking roles; its progress reaches the DOM through a variable instead.
  // It is re-checked on render rather than dropped in an effect: a landing, or a layout change
  // it cannot outlive, simply stops matching the slot and mode it was made for
  const [fadePair, setFadePair] = useState<{ layer: DragFade; key: string } | null>(null)
  const resting = transition === 'next' ? spreads.length : transition === 'previous' ? -1 : shownIndex
  const fadeKey = `${pageLayout}|${direction}|${splitOn}|${splitHalf}|${crossfade}|${book.id}|${transition ?? ''}`
  const fade =
    fadePair !== null && fadePair.key === fadeKey && fadePair.layer.base === resting ? fadePair.layer : null
  const fadeIn = fade === null ? -2 : fade.base + (fade.dir === 'next' ? 1 : -1)
  const onFade = useCallback(
    (layer: DragFade | null) => setFadePair(layer === null ? null : { layer, key: fadeKey }),
    [fadeKey],
  )
  const canPaint = useCallback(
    (dir: DragFadeDir) => {
      const i = resting + (dir === 'next' ? 1 : -1)
      // a transition page is not a spread: there is no image to wait for
      if (i < 0 || i >= spreads.length) return true
      return spreads[i].every((p) => p.blank || readyPages.has(p.url))
    },
    [resting, spreads, readyPages],
  )

  const {
    targetRef,
    zoomed,
    touchAction: pinchTouchAction,
    onPointerDown: pinchPointerDown,
    onPointerMove: pinchPointerMove,
    onPointerUp: pinchPointerUp,
    onPointerCancel: pinchPointerCancel,
    gestureConsumed,
    consumeClick,
    reset: resetPinch,
  } = usePinchZoom({ zoomedTouchAction: 'none' })
  // layout shifts (page turn, fit mode, direction, spread re-pairing, half swap) leave stale pan offsets behind
  useLayoutEffect(() => {
    resetPinch()
  }, [spreadIndex, spreadKey, splitHalf, splitOn, scale, pageLayout, direction, transition, resetPinch])

  const goSpread = useCallback((i: number) => onPageChange(spreadCurrentPage(spreads[i])), [spreads, onPageChange])
  const prev = useCallback(() => {
    if (transition === 'next') setTransition(null)
    else if (transition === 'previous') onLeaveBook('previous')
    else if (curSplittable && splitHalf === 1) {
      onSplitHalfChange(0)
    } else if (spreadIndex > 0) {
      const target = spreads[spreadIndex - 1]
      goSpread(spreadIndex - 1)
      // landing on a wide page from after it means its second half shows first
      if (splitOn && target.length === 1 && !target[0].blank && isLandscape(target[0])) onSplitHalfChange(1)
    } else setTransition('previous')
  }, [transition, curSplittable, splitHalf, splitOn, spreadIndex, spreads, goSpread, onLeaveBook, onSplitHalfChange])
  const next = useCallback(() => {
    if (transition === 'previous') setTransition(null)
    else if (transition === 'next') onLeaveBook('next')
    else if (curSplittable && splitHalf === 0) onSplitHalfChange(1)
    else if (spreadIndex < spreads.length - 1) goSpread(spreadIndex + 1)
    else setTransition('next')
  }, [transition, curSplittable, splitHalf, spreadIndex, spreads.length, goSpread, onLeaveBook, onSplitHalfChange])
  const turnLeft = useCallback(() => {
    if (!vertical) (flip ? next : prev)()
  }, [vertical, flip, next, prev])
  const turnRight = useCallback(() => {
    if (!vertical) (flip ? prev : next)()
  }, [vertical, flip, next, prev])

  useWindowKeys((e) => {
    switch (e.key) {
      case 'ArrowLeft':
        turnLeft()
        break
      case 'ArrowRight':
        turnRight()
        break
      case 'ArrowUp':
        if (vertical) prev()
        break
      case 'ArrowDown':
        if (vertical) next()
        break
    }
  })

  // probe the current spread and two spreads ahead of the ±2 render window; the probes
  // double as dimension sources for pages the server has not measured, and the header
  // poll settles the current page's pairing before its image has finished loading
  useEffect(() => {
    for (const i of [spreadIndex, spreadIndex + 3, spreadIndex + 4]) {
      spreads[i]?.forEach((p) => {
        if (!p.blank) {
          const img = new Image()
          img.onload = () => onMeasure(p.number, img.naturalWidth, img.naturalHeight)
          measureWhenKnown(img, (w, h) => onMeasure(p.number, w, h))
          img.src = p.url
        }
      })
    }
  }, [spreadIndex, spreads, onMeasure])

  const wheelRef = useRef<{ delta: number; locked: boolean; timer?: number }>({ delta: 0, locked: false })
  useEffect(() => {
    const wheel = wheelRef.current
    return () => window.clearTimeout(wheel.timer)
  }, [])
  const onWheel = (e: React.WheelEvent<HTMLDivElement>) => {
    if (scrollable || zoomed || e.ctrlKey || e.metaKey || e.altKey || e.shiftKey) return
    if (Math.abs(e.deltaY) <= Math.abs(e.deltaX)) return

    const wheel = wheelRef.current
    window.clearTimeout(wheel.timer)
    // trackpads emit long inertial streams, so a turn stays locked until the wheel goes quiet
    wheel.timer = window.setTimeout(() => {
      wheel.delta = 0
      wheel.locked = false
    }, 200)
    if (wheel.locked) return

    const unit = e.deltaMode === 1 ? 16 : e.deltaMode === 2 ? e.currentTarget.clientHeight : 1
    const delta = e.deltaY * unit
    if (wheel.delta * delta < 0) wheel.delta = 0
    wheel.delta += delta
    if (Math.abs(wheel.delta) < 48) return

    wheel.delta = 0
    wheel.locked = true
    ;(delta > 0 ? next : prev)()
  }

  // a swipe consumes the click that the browser fires after pointerup
  const swipedRef = useRef(false)
  const trackRef = useRef<HTMLDivElement>(null)
  // the drag follows the finger only where the turn axis does not scroll natively (fit modes)
  // and the track actually slides; elsewhere the swipe keeps its release-only thresholds, and a
  // crossfade at least fades the old page away under it
  const followDrag = !crossfade && (vertical ? !scrollable : !hugContent)
  const drag = useDragTurn({
    track: () => trackRef.current,
    vertical,
    flip,
    follow: followDrag,
    fade: crossfade,
    canPaint,
    onFade,
    animate,
    resting,
    spreadCount: spreads.length,
    followForward: !(curSplittable && splitHalf === 0),
    followBackward: !(curSplittable && splitHalf === 1),
    swipe,
    onNext: next,
    onPrevious: prev,
    onLeave: onLeaveBook,
    gestureConsumed,
    markSwiped: () => {
      swipedRef.current = true
    },
  })
  // following the finger needs the turn axis taken away from native scrolling
  const touchAction =
    !zoomed && swipe && followDrag ? (vertical ? 'pan-x' : 'pan-y') : pinchTouchAction
  const onPointerDown = (e: React.PointerEvent<HTMLDivElement>) => {
    // a touch that moved fires no click, so a flag left by the last drag must not eat this tap
    if (e.isPrimary) swipedRef.current = false
    pinchPointerDown(e)
    drag.onPointerDown(e)
  }
  const onPointerMove = (e: React.PointerEvent<HTMLDivElement>) => {
    pinchPointerMove(e)
    drag.onPointerMove(e)
  }
  const onPointerUp = (e: React.PointerEvent<HTMLDivElement>) => {
    pinchPointerUp(e)
    drag.onPointerUp(e)
  }
  const onPointerCancel = (e: React.PointerEvent<HTMLDivElement>) => {
    pinchPointerCancel(e)
    drag.onPointerCancel()
  }

  // click regions instead of overlay zones: overlays would swallow wheel scrolling
  const onClick = (e: React.MouseEvent<HTMLDivElement>) => {
    if (swipedRef.current) {
      swipedRef.current = false
      return
    }
    if (consumeClick()) return
    // while zoomed a tap only toggles chrome; a page turn would dump the zoom
    if (zoomed) {
      onToggleChrome()
      return
    }
    const rect = e.currentTarget.getBoundingClientRect()
    const x = (e.clientX - rect.left) / rect.width
    const y = (e.clientY - rect.top) / rect.height
    const action = vertical ? tapAction(tapZones, y, x, false) : tapAction(tapZones, x, y, flip)
    if (action === 'previous') prev()
    else if (action === 'next') next()
    else onToggleChrome()
  }

  const offset = (slot: number) =>
    vertical ? `translateY(${slot * 100}%)` : `translateX(${(flip ? -1 : 1) * slot * 100}%)`
  // transition pages have no spread of their own: they sit one slot past either end of the track
  const transitionSlot = (dir: 'previous' | 'next') => (dir === 'next' ? spreads.length : -1)
  const transform = offset(-(transition ? transitionSlot(transition) : shownIndex))
  const hiddenSlide = 'pointer-events-none z-10 opacity-0 transition-opacity duration-200'

  return (
    <div
      className="h-full w-full overflow-hidden"
      style={{ touchAction }}
      onClick={onClick}
      onWheel={onWheel}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerUp}
      onPointerCancel={onPointerCancel}
    >
      <div
        ref={targetRef}
        className="h-full w-full"
        style={{ transformOrigin: '0 0', willChange: zoomed ? 'transform' : undefined }}
      >
        <div
          // remounted on a mode switch, or every slide would fade out from the strip's opaque state
          key={crossfade ? 'crossfade' : 'strip'}
          ref={trackRef}
          className={cn(
            'relative h-full w-full',
            !crossfade && 'flex',
            !crossfade && (vertical ? 'flex-col' : flip && 'flex-row-reverse'),
            !crossfade && animate && 'transition-transform duration-200 ease-[cubic-bezier(.25,.8,.5,1)]',
            // a drag writes the transform per frame; the transition would lag it behind the finger
            '[&.dragging]:transition-none',
          )}
          style={crossfade ? undefined : { transform }}
        >
          {spreads.map((spread, i) => {
            const double = spread.length > 1
            const near = Math.abs(i - spreadIndex) <= 2 || i === shownIndex
            const fadingOut = fade !== null && fade.base === i
            const fadingIn = fade !== null && fadeIn === i
            const fadeStyle = fade !== null && fadingOut ? fadeLayerStyle(fade) : undefined
            return (
              <div
                key={i}
                data-reader-scroll=""
                className={cn(
                  'flex h-full w-full overflow-auto no-scrollbar',
                  !crossfade && 'shrink-0',
                  crossfade && 'absolute inset-0',
                  // the drag draws its own pair: the target underneath, the outgoing on top
                  fadingOut && 'pointer-events-none z-10',
                  fadingIn && 'pointer-events-none z-0',
                  // the new page is opaque at once under the fading old one, so the
                  // reader background never shows through mid-turn
                  crossfade && !fadingOut && !fadingIn && (transition !== null || i !== shownIndex) && hiddenSlide,
                )}
                style={fadeStyle}
              >
                <div
                  className={cn(
                    'm-auto flex justify-center',
                    flip && !vertical && 'flex-row-reverse',
                    hugContent ? 'w-max min-w-full shrink-0' : 'w-full',
                    scrollable ? 'items-start' : 'items-center',
                  )}
                >
                  {spread.map((p, j) => {
                    // only the current spread crops; neighbours render whole pages so the
                    // slide animation shows the destination page before its half applies
                    if (i === spreadIndex && curSplittable && p.width && p.height) {
                      // object-position swaps halves on the same element, so no image reload
                      const side = (flip ? ['right', 'left'] : ['left', 'right'])[splitHalf]
                      return (
                        <PageImage
                          key={p.number}
                          src={p.url}
                          alt={t('pageAlt', { page: p.number })}
                          className="shrink-0 overflow-hidden"
                          style={splitBoxStyle(scale, p.width, p.height)}
                          imgClassName="block h-full w-full object-cover"
                          imgStyle={{ objectPosition: `${side} center` }}
                          onMeasure={(w, h) => onMeasure(p.number, w, h)}
                        />
                      )
                    }
                    const measured = !!(p.width && p.height)
                    const cls = slotClass(scale, double, measured)
                    // every slot keeps an aspect-ratio box (real dims once measured, a 2:3
                    // fallback before) so an unloaded page holds its own half instead of
                    // collapsing and letting the loaded mate sit in the spread's middle
                    const dim = slotStyle(scale, double, stretchSmall, p)
                    if (!near && !p.blank) return <div key={p.number} aria-hidden className={cls} style={dim} />
                    if (p.blank)
                      return (
                        <img
                          key={`blank-${j}`}
                          src={p.url}
                          alt=""
                          draggable={false}
                          className={cls}
                          style={{ ...dim, filter: pageFilter }}
                        />
                      )
                    return (
                      <PageImage
                        key={p.number}
                        src={p.url}
                        alt={t('pageAlt', { page: p.number })}
                        className={cls}
                        style={dim}
                        predecode={i - spreadIndex >= -1 && i - spreadIndex <= 2}
                        onReady={markReady}
                        onMeasure={(w, h) => onMeasure(p.number, w, h)}
                      />
                    )
                  })}
                </div>
              </div>
            )
          })}
          {(['previous', 'next'] as const).map((dir) => {
            const slot = transitionSlot(dir)
            const fadingOut = fade !== null && fade.base === slot
            const fadingIn = fade !== null && fadeIn === slot
            const fadeStyle = fade !== null && fadingOut ? fadeLayerStyle(fade) : undefined
            return (
              (dir === 'next' ? atLast : atFirst) && (
                <div
                  key={dir}
                  className={cn(
                    'absolute inset-0',
                    fadingOut && 'pointer-events-none z-10',
                    fadingIn && 'pointer-events-none z-0',
                    crossfade && !fadingOut && !fadingIn && transition !== dir && hiddenSlide,
                  )}
                  style={crossfade ? fadeStyle : { transform: offset(slot) }}
                >
                  <BookTransition
                    direction={dir}
                    book={book}
                    target={dir === 'next' ? nextBook : previousBook}
                    flip={flip}
                    onLeave={() => onLeaveBook(dir)}
                  />
                </div>
              )
            )
          })}
        </div>
      </div>
    </div>
  )
}
