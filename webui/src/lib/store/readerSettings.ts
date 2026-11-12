import { useSyncExternalStore } from 'react'
import { create } from 'zustand'
import { persist } from 'zustand/middleware'
import type { ReadingDirection } from '@/lib/api/types'
import { useUiStore } from '@/lib/store/ui'
import type { PagedReaderLayout } from '@/lib/utils/spreads'

export type ScaleType = 'SCREEN' | 'WIDTH' | 'WIDTH_SHRINK_ONLY' | 'HEIGHT' | 'ORIGINAL'
export type ContinuousScaleType = 'WIDTH' | 'ORIGINAL'
export type PageTransition = 'SLIDE' | 'FADE'
export type ReaderBackground = 'BLACK' | 'GRAY' | 'WHITE' | 'AUTO'
export type ResolvedReaderBackground = Exclude<ReaderBackground, 'AUTO'>
export type TapZoneLayout = 'STANDARD' | 'REVERSED' | 'EDGE' | 'KINDLE' | 'OFF'
export type EpubTheme = 'DAY' | 'SEPIA' | 'NIGHT'
export type EpubFontFamily = 'Original' | `server:${string}` | `local:${string}`

export interface ReaderSettings {
  scale: ScaleType
  pageLayout: PagedReaderLayout
  /** split wide pages into two half-page turns in single-page layout */
  splitWide: boolean
  /** upscale pages smaller than the viewport in fit-screen mode */
  stretchSmall: boolean
  continuousScale: ContinuousScaleType
  /** side padding percent, 0-40 step 5 */
  continuousPadding: number
  /** gap between pages in px */
  continuousMargin: number
  readingDirection: ReadingDirection
  swipe: boolean
  tapZones: TapZoneLayout
  animations: boolean
  /** paged reader turn style while animations are on */
  pageTransition: PageTransition
  alwaysFullscreen: boolean
  background: ReaderBackground
  epubTheme: EpubTheme
  epubScroll: boolean
  epubFontFamily: EpubFontFamily
  /** multiplier over the publisher font size, 1 keeps the book default */
  epubFontSize: number
  /** null keeps the publisher default */
  epubLineHeight: number | null
}

interface ReaderSettingsState extends ReaderSettings {
  update: (patch: Partial<ReaderSettings>) => void
}

export const READER_BACKGROUNDS: Record<ResolvedReaderBackground, string> = {
  BLACK: '#000000',
  GRAY: '#161618',
  WHITE: '#f5f4f0',
}

export const useReaderSettings = create<ReaderSettingsState>()(
  persist(
    (set) => ({
      scale: 'SCREEN',
      pageLayout: 'SINGLE_PAGE',
      splitWide: false,
      stretchSmall: false,
      continuousScale: 'WIDTH',
      continuousPadding: 0,
      continuousMargin: 0,
      readingDirection: 'LEFT_TO_RIGHT',
      swipe: true,
      tapZones: 'STANDARD',
      animations: true,
      pageTransition: 'SLIDE',
      alwaysFullscreen: false,
      background: 'BLACK',
      epubTheme: 'DAY',
      epubScroll: false,
      epubFontFamily: 'Original',
      epubFontSize: 1,
      epubLineHeight: null,
      update: (patch) => set(patch),
    }),
    {
      name: 'kmweb.reader',
      // the turn style is an explicit choice now; a stored AUTO (the old landscape crossfade
      // preset) has no segment left to select, so it lands on the default slide
      version: 1,
      migrate: (persisted) => {
        const state = (persisted ?? {}) as Partial<ReaderSettingsState> & { pageTransition?: string }
        return { ...state, pageTransition: state.pageTransition === 'FADE' ? 'FADE' : 'SLIDE' } as ReaderSettingsState
      },
    },
  ),
)

const systemLight = window.matchMedia('(prefers-color-scheme: light)')

/** The background to paint: AUTO resolves against the app theme and tracks it live. */
export function useReaderBackground(): ResolvedReaderBackground {
  const background = useReaderSettings((s) => s.background)
  const theme = useUiStore((s) => s.theme)
  const prefersLight = useSyncExternalStore(
    (onChange) => {
      systemLight.addEventListener('change', onChange)
      return () => systemLight.removeEventListener('change', onChange)
    },
    () => systemLight.matches,
  )
  if (background !== 'AUTO') return background
  return (theme === 'system' ? prefersLight : theme === 'light') ? 'WHITE' : 'BLACK'
}
