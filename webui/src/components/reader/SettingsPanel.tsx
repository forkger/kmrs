import { useEffect, useRef, type ReactNode } from 'react'
import * as RadixDialog from '@radix-ui/react-dialog'
import { AnimatePresence, motion, useReducedMotion } from 'motion/react'
import { useTranslation } from 'react-i18next'
import {
  ArrowDown,
  ArrowLeft,
  ArrowRight,
  ArrowsHorizontal,
  ArrowsInLineHorizontal,
  ArrowsVertical,
  BookOpen,
  BookOpenText,
  CircleHalf,
  CornersOut,
  File,
  FrameCorners,
  Scroll,
  X,
} from '@phosphor-icons/react'
import type { ReadingDirection } from '@/lib/api/types'
import {
  READER_BACKGROUNDS,
  useReaderSettings,
  type ContinuousScaleType,
  type PageTransition,
  type ReaderBackground,
  type ScaleType,
  type TapZoneLayout,
} from '@/lib/store/readerSettings'
import {
  DEFAULT_FILTERS,
  FILTER_BLEND_MODES,
  FILTER_LIMITS,
  hasActiveFilters,
  TINT_ALPHA_LIMIT,
  TINT_CHANNEL_LIMIT,
  type FilterLevel,
  type FilterLimit,
  type ReaderFilters,
  type TintFilter,
} from '@/lib/utils/readerFilters'
import type { PagedReaderLayout } from '@/lib/utils/spreads'
import { IconButton } from '@/components/ui/IconButton'
import { Button } from '@/components/ui/Button'
import { SegmentedControl } from '@/components/ui/SegmentedControl'
import { Slider } from '@/components/ui/Slider'
import { Switch } from '@/components/ui/Switch'
import { Chip, FieldGroup, OptionTiles, Row, RowGroup, Section, SliderField } from '@/components/reader/panel'
import { useEffectiveControls, useEffectiveSettings } from '@/components/reader/effectiveSettings'
import { cn } from '@/lib/utils/cn'
import { tapAction, type TapAction } from './tapZones'

const SCALE_LABEL_KEYS: Record<ScaleType, string> = {
  SCREEN: 'scale.screen',
  WIDTH: 'scale.width',
  WIDTH_SHRINK_ONLY: 'scale.shrink',
  HEIGHT: 'scale.height',
  ORIGINAL: 'scale.original',
}

const LAYOUT_LABEL_KEYS: Record<PagedReaderLayout, string> = {
  SINGLE_PAGE: 'layout.single',
  DOUBLE_PAGES: 'layout.double',
  DOUBLE_NO_COVER: 'layout.doubleNoCover',
}

const DIRECTION_LABEL_KEYS: Record<ReadingDirection, string> = {
  LEFT_TO_RIGHT: 'common:readingDirection.leftToRight',
  RIGHT_TO_LEFT: 'common:readingDirection.rightToLeft',
  VERTICAL: 'common:readingDirection.vertical',
  WEBTOON: 'common:readingDirection.webtoon',
}

const BACKGROUND_LABEL_KEYS: Record<ReaderBackground, string> = {
  BLACK: 'background.black',
  GRAY: 'background.gray',
  WHITE: 'background.white',
  AUTO: 'background.auto',
}

const SCALE_ICONS: Record<ScaleType, ReactNode> = {
  SCREEN: <CornersOut />,
  WIDTH: <ArrowsHorizontal />,
  WIDTH_SHRINK_ONLY: <ArrowsInLineHorizontal />,
  HEIGHT: <ArrowsVertical />,
  ORIGINAL: <FrameCorners />,
}

const LAYOUT_ICONS: Record<PagedReaderLayout, ReactNode> = {
  SINGLE_PAGE: <File />,
  DOUBLE_PAGES: <BookOpen />,
  DOUBLE_NO_COVER: <BookOpenText />,
}

const DIRECTION_ICONS: Record<ReadingDirection, ReactNode> = {
  LEFT_TO_RIGHT: <ArrowRight />,
  RIGHT_TO_LEFT: <ArrowLeft />,
  VERTICAL: <ArrowDown />,
  WEBTOON: <Scroll />,
}

const TAP_ZONE_LABEL_KEYS: Record<TapZoneLayout, string> = {
  STANDARD: 'tapZones.standard',
  REVERSED: 'tapZones.reversed',
  EDGE: 'tapZones.edge',
  KINDLE: 'tapZones.kindle',
  OFF: 'tapZones.off',
}

const TAP_ACTION_CLASSES: Record<TapAction, string> = {
  previous: 'fill-[#a4706f]',
  next: 'fill-[#6f9a80]',
  menu: 'fill-[#6f86a8]',
}

function PanelSection({ children, className }: { children: ReactNode; className?: string }) {
  const reduceMotion = useReducedMotion()
  return (
    <motion.div
      initial={reduceMotion ? false : { height: 0, opacity: 0 }}
      animate={{ height: 'auto', opacity: 1 }}
      exit={reduceMotion ? { opacity: 0 } : { height: 0, opacity: 0 }}
      transition={{ duration: reduceMotion ? 0 : 0.18, ease: [0.16, 1, 0.3, 1] }}
      className={cn('overflow-hidden', className)}
    >
      {children}
    </motion.div>
  )
}

/** Samples the real tap resolver, so a preview can never drift from what a tap does. */
function TapZonePreview({ layout, direction }: { layout: TapZoneLayout; direction: ReadingDirection }) {
  const vertical = direction === 'VERTICAL'
  // fine enough to land on every zone boundary: 5% along the turn axis, 25% across it
  const [columns, rows] = vertical ? [4, 20] : [20, 4]
  const cells = Array.from({ length: columns * rows }, (_, i) => {
    const x = i % columns
    const y = Math.floor(i / columns)
    const [left, top] = [(x + 0.5) / columns, (y + 0.5) / rows]
    const action = vertical
      ? tapAction(layout, top, left, false)
      : tapAction(layout, left, top, direction === 'RIGHT_TO_LEFT')
    return <rect key={i} x={x} y={y} width={1} height={1} className={TAP_ACTION_CLASSES[action]} />
  })
  return (
    <svg
      aria-hidden
      viewBox={`0 0 ${columns} ${rows}`}
      preserveAspectRatio="none"
      shapeRendering="crispEdges"
      className="aspect-[2/3] h-12 rounded-sm landscape:aspect-[3/2]"
    >
      {cells}
    </svg>
  )
}

const PAGED_SCALE_VALUES: ScaleType[] = ['SCREEN', 'WIDTH', 'WIDTH_SHRINK_ONLY', 'HEIGHT', 'ORIGINAL']
const CONTINUOUS_SCALE_VALUES: ContinuousScaleType[] = ['WIDTH', 'ORIGINAL']
const LAYOUT_VALUES: PagedReaderLayout[] = ['SINGLE_PAGE', 'DOUBLE_PAGES', 'DOUBLE_NO_COVER']
const BACKGROUND_VALUES: ReaderBackground[] = ['AUTO', 'BLACK', 'GRAY', 'WHITE']
const PAGE_TRANSITION_VALUES: PageTransition[] = ['SLIDE', 'FADE']
const PAGE_TRANSITION_LABEL_KEYS: Record<PageTransition, string> = {
  SLIDE: 'pageTransition.slide',
  FADE: 'pageTransition.fade',
}

/** One numeric filter (brightness etc.): a switch, with its slider expanding underneath while enabled. */
function FilterLevelField({
  label,
  limit,
  level,
  format,
  onChange,
}: {
  label: string
  limit: FilterLimit
  level: FilterLevel
  format: (value: number) => string
  onChange: (level: FilterLevel) => void
}) {
  return (
    <div className="space-y-1.5">
      <div className="flex min-h-6 items-center justify-between text-[13px] text-ink">
        <span>{label}</span>
        <Switch checked={level.enabled} onCheckedChange={(enabled) => onChange({ ...level, enabled })} label={label} />
      </div>
      <AnimatePresence initial={false}>
        {level.enabled && (
          <PanelSection>
            <div className="flex items-center gap-3">
              <Slider
                className="flex-1"
                value={level.value}
                onValueChange={(value) => onChange({ ...level, value })}
                min={limit.min}
                max={limit.max}
                step={limit.step}
                label={label}
              />
              <span className="w-10 text-right font-mono text-xs text-ink-3">{format(level.value)}</span>
            </div>
          </PanelSection>
        )}
      </AnimatePresence>
    </div>
  )
}

interface SettingsPanelProps {
  open: boolean
  direction: ReadingDirection
  onDirectionChange: (direction: ReadingDirection) => void
  onAlwaysFullscreenChange: (on: boolean) => void
  onClose: () => void
}

export function SettingsPanel({
  open,
  direction,
  onDirectionChange,
  onAlwaysFullscreenChange,
  onClose,
}: SettingsPanelProps) {
  // content-dependent settings resolve through the effective context and write
  // back as per-series overrides; preferences stay global and write the store directly
  const { scale, pageLayout, splitWide, filters } = useEffectiveSettings()
  const { overriddenKeys, update, resetOverrides } = useEffectiveControls()
  const tapZones = useReaderSettings((s) => s.tapZones)
  const stretchSmall = useReaderSettings((s) => s.stretchSmall)
  const animations = useReaderSettings((s) => s.animations)
  const pageTransition = useReaderSettings((s) => s.pageTransition)
  const background = useReaderSettings((s) => s.background)
  const continuousScale = useReaderSettings((s) => s.continuousScale)
  const continuousPadding = useReaderSettings((s) => s.continuousPadding)
  const continuousMargin = useReaderSettings((s) => s.continuousMargin)
  const swipe = useReaderSettings((s) => s.swipe)
  const alwaysFullscreen = useReaderSettings((s) => s.alwaysFullscreen)
  const updateGlobal = useReaderSettings((s) => s.update)
  const reduceMotion = useReducedMotion()
  const closeRef = useRef<HTMLButtonElement>(null)
  const returnFocusRef = useRef<HTMLElement | null>(null)
  const { t } = useTranslation('reader')

  const webtoon = direction === 'WEBTOON'
  const updateFilters = (patch: Partial<ReaderFilters>) => update({ filters: { ...filters, ...patch } })
  const updateTint = (patch: Partial<TintFilter>) =>
    update({ filters: { ...filters, tint: { ...filters.tint, ...patch } } })
  useEffect(
    () => () => {
      ;(returnFocusRef.current ?? document.querySelector<HTMLElement>('[aria-keyshortcuts="s"]'))?.focus()
    },
    [],
  )

  const directionOptions = (Object.keys(DIRECTION_LABEL_KEYS) as ReadingDirection[]).map((value) => ({
    value,
    label: t(DIRECTION_LABEL_KEYS[value]),
    icon: DIRECTION_ICONS[value],
  }))
  const pagedScaleOptions = PAGED_SCALE_VALUES.map((value) => ({
    value,
    label: t(SCALE_LABEL_KEYS[value]),
    icon: SCALE_ICONS[value],
  }))
  const continuousScaleOptions = CONTINUOUS_SCALE_VALUES.map((value) => ({
    value,
    label: t(SCALE_LABEL_KEYS[value]),
    icon: SCALE_ICONS[value],
  }))
  const layoutOptions = LAYOUT_VALUES.map((value) => ({
    value,
    label: t(LAYOUT_LABEL_KEYS[value]),
    icon: LAYOUT_ICONS[value],
  }))
  const tapZoneOptions = (Object.keys(TAP_ZONE_LABEL_KEYS) as TapZoneLayout[]).map((value) => ({
    value,
    label: t(TAP_ZONE_LABEL_KEYS[value]),
    icon: <TapZonePreview layout={value} direction={direction} />,
  }))
  const transitionOptions = PAGE_TRANSITION_VALUES.map((value) => ({
    value,
    label: t(PAGE_TRANSITION_LABEL_KEYS[value]),
  }))

  return (
    <RadixDialog.Root open={open} onOpenChange={(value) => !value && onClose()}>
      <AnimatePresence>
        {open && (
          <RadixDialog.Portal forceMount>
            <RadixDialog.Overlay asChild>
              <motion.div
                className="fixed inset-0 z-30 bg-black/40"
                initial={{ opacity: 0 }}
                animate={{ opacity: 1 }}
                exit={{ opacity: 0 }}
                transition={{ duration: reduceMotion ? 0 : 0.2 }}
              />
            </RadixDialog.Overlay>
            <RadixDialog.Content
              asChild
              onOpenAutoFocus={(event) => {
                event.preventDefault()
                returnFocusRef.current = document.activeElement instanceof HTMLElement ? document.activeElement : null
                closeRef.current?.focus()
              }}
              onCloseAutoFocus={(event) => {
                event.preventDefault()
                ;(returnFocusRef.current ?? document.querySelector<HTMLElement>('[aria-keyshortcuts="s"]'))?.focus()
              }}
            >
              <motion.aside
                initial={{ x: '100%' }}
                animate={{ x: 0 }}
                exit={{ x: '100%' }}
                transition={{ duration: reduceMotion ? 0 : 0.25, ease: [0.16, 1, 0.3, 1] }}
                className="theme-dark fixed top-0 right-0 z-30 flex h-full w-80 max-w-[85vw] safe-top safe-bottom flex-col border-l border-line bg-bg/90 text-ink backdrop-blur-xl"
              >
                <div className="flex shrink-0 items-center justify-between border-b border-line px-4 py-3">
                  <RadixDialog.Title className="text-[15px] font-semibold">{t('panel.title')}</RadixDialog.Title>
                  <RadixDialog.Close asChild>
                    <IconButton
                      ref={closeRef}
                      label={t('panel.close')}
                      className="text-ink-2 hover:bg-overlay hover:text-ink"
                    >
                      <X className="size-4" />
                    </IconButton>
                  </RadixDialog.Close>
                </div>

                <div className="min-h-0 flex-1 space-y-5 overflow-y-auto px-3 py-4">
              {overriddenKeys.length > 0 && (
                <div className="flex items-center justify-between gap-3 rounded-xl bg-raised px-3 py-2">
                  <span className="text-[13px] text-ink-2">{t('panel.customized')}</span>
                  <Button size="sm" onClick={resetOverrides}>
                    {t('panel.resetOverrides')}
                  </Button>
                </div>
              )}
              <Section title={t('panel.direction')}>
                <OptionTiles options={directionOptions} value={direction} onChange={onDirectionChange} perRow={4} />
              </Section>

              <Section title={t('panel.scale')}>
                {webtoon ? (
                  <OptionTiles
                    options={continuousScaleOptions}
                    value={continuousScale}
                    onChange={(v) => updateGlobal({ continuousScale: v })}
                    perRow={2}
                  />
                ) : (
                  <OptionTiles
                    options={pagedScaleOptions}
                    value={scale}
                    onChange={(v) => update({ scale: v })}
                    perRow={3}
                  />
                )}
                <AnimatePresence initial={false}>
                  {!webtoon && scale === 'SCREEN' && (
                    <PanelSection className="mt-2">
                      <RowGroup>
                        <Row label={t('panel.stretchSmall')}>
                          <Switch
                            checked={stretchSmall}
                            onCheckedChange={(v) => updateGlobal({ stretchSmall: v })}
                            label={t('panel.stretchSmall')}
                          />
                        </Row>
                      </RowGroup>
                    </PanelSection>
                  )}
                </AnimatePresence>
              </Section>

              <AnimatePresence initial={false}>
                {!webtoon && (
                  <PanelSection>
                    <Section title={t('panel.layout')}>
                      <OptionTiles
                        options={layoutOptions}
                        value={pageLayout}
                        onChange={(v) => update({ pageLayout: v })}
                        perRow={3}
                      />
                      <RowGroup>
                        <Row label={t('panel.splitWide')}>
                          <Switch
                            checked={splitWide}
                            onCheckedChange={(v) => update({ splitWide: v })}
                            label={t('panel.splitWide')}
                          />
                        </Row>
                      </RowGroup>
                    </Section>
                  </PanelSection>
                )}
              </AnimatePresence>

              <AnimatePresence initial={false}>
                {!webtoon && (
                  <PanelSection>
                    <Section title={t('panel.tapZones')}>
                      <OptionTiles
                        options={tapZoneOptions}
                        value={tapZones}
                        onChange={(v) => updateGlobal({ tapZones: v })}
                        perRow={3}
                      />
                      <div className="flex justify-center gap-4 text-[11px] text-ink-2">
                        {(Object.keys(TAP_ACTION_CLASSES) as TapAction[]).map((action) => (
                          <span key={action} className="flex items-center gap-1.5">
                            <svg aria-hidden viewBox="0 0 1 1" className="size-2.5 rounded-sm">
                              <rect width={1} height={1} className={TAP_ACTION_CLASSES[action]} />
                            </svg>
                            {t(`tapZones.${action}`)}
                          </span>
                        ))}
                      </div>
                    </Section>
                  </PanelSection>
                )}
              </AnimatePresence>

              <Section title={t('panel.behavior')}>
                <RowGroup>
                  <Row label={t('panel.background')}>
                    <div className="flex gap-2.5">
                      {BACKGROUND_VALUES.map((value) => (
                        <button
                          key={value}
                          type="button"
                          aria-label={t(BACKGROUND_LABEL_KEYS[value])}
                          aria-pressed={background === value}
                          title={t(BACKGROUND_LABEL_KEYS[value])}
                          onClick={() => updateGlobal({ background: value })}
                          className={cn(
                            'size-6 cursor-pointer rounded-full text-ink-2',
                            value !== 'AUTO' && 'border border-line-strong',
                            background === value && 'ring-2 ring-accent ring-offset-2 ring-offset-raised',
                          )}
                          style={value === 'AUTO' ? undefined : { background: READER_BACKGROUNDS[value] }}
                        >
                          {value === 'AUTO' && <CircleHalf aria-hidden weight="fill" className="size-full" />}
                        </button>
                      ))}
                    </div>
                  </Row>
                  <Row label={t('panel.swipe')}>
                    <Switch checked={swipe} onCheckedChange={(v) => updateGlobal({ swipe: v })} label={t('panel.swipe')} />
                  </Row>
                  <Row label={t('panel.animations')}>
                    <Switch
                      checked={animations}
                      onCheckedChange={(v) => updateGlobal({ animations: v })}
                      label={t('panel.animations')}
                    />
                  </Row>
                  <AnimatePresence initial={false}>
                    {animations && !webtoon && (
                      <PanelSection>
                        <Row label={t('panel.pageTransition')}>
                          <SegmentedControl
                            options={transitionOptions}
                            value={pageTransition}
                            onChange={(v) => updateGlobal({ pageTransition: v })}
                            size="sm"
                          />
                        </Row>
                      </PanelSection>
                    )}
                  </AnimatePresence>
                  <Row label={t('panel.alwaysFullscreen')}>
                    <Switch
                      checked={alwaysFullscreen}
                      onCheckedChange={onAlwaysFullscreenChange}
                      label={t('panel.alwaysFullscreen')}
                    />
                  </Row>
                </RowGroup>
              </Section>

              <Section title={t('panel.filters')}>
                <FieldGroup>
                  {(Object.keys(FILTER_LIMITS) as Array<keyof typeof FILTER_LIMITS>).map((key) => (
                    <FilterLevelField
                      key={key}
                      label={t(`filter.${key}`)}
                      limit={FILTER_LIMITS[key]}
                      level={filters[key]}
                      format={key === 'hue' ? (v) => `${v}°` : (v) => `${v}%`}
                      onChange={(level) => updateFilters({ [key]: level })}
                    />
                  ))}
                </FieldGroup>
                <RowGroup>
                  <Row label={t('filter.sepia')}>
                    <Switch
                      checked={filters.sepia}
                      onCheckedChange={(sepia) => updateFilters({ sepia })}
                      label={t('filter.sepia')}
                    />
                  </Row>
                  <Row label={t('filter.grayscale')}>
                    <Switch
                      checked={filters.grayscale}
                      onCheckedChange={(grayscale) => updateFilters({ grayscale })}
                      label={t('filter.grayscale')}
                    />
                  </Row>
                  <Row label={t('filter.invert')}>
                    <Switch
                      checked={filters.invert}
                      onCheckedChange={(invert) => updateFilters({ invert })}
                      label={t('filter.invert')}
                    />
                  </Row>
                </RowGroup>
                <FieldGroup>
                  <div className="flex min-h-6 items-center justify-between text-[13px] text-ink">
                    <span>{t('filter.tint')}</span>
                    <Switch
                      checked={filters.tint.enabled}
                      onCheckedChange={(enabled) => updateTint({ enabled })}
                      label={t('filter.tint')}
                    />
                  </div>
                  <AnimatePresence initial={false}>
                    {filters.tint.enabled && (
                      <PanelSection>
                        <div className="space-y-4">
                          {(['red', 'green', 'blue'] as const).map((channel) => (
                            <SliderField
                              key={channel}
                              label={t(`filter.${channel}`)}
                              value={String(filters.tint[channel])}
                            >
                              <Slider
                                value={filters.tint[channel]}
                                onValueChange={(v) => updateTint({ [channel]: v })}
                                min={TINT_CHANNEL_LIMIT.min}
                                max={TINT_CHANNEL_LIMIT.max}
                                step={TINT_CHANNEL_LIMIT.step}
                                label={t(`filter.${channel}`)}
                              />
                            </SliderField>
                          ))}
                          <SliderField label={t('filter.alpha')} value={`${filters.tint.alpha}%`}>
                            <Slider
                              value={filters.tint.alpha}
                              onValueChange={(alpha) => updateTint({ alpha })}
                              min={TINT_ALPHA_LIMIT.min}
                              max={TINT_ALPHA_LIMIT.max}
                              step={TINT_ALPHA_LIMIT.step}
                              label={t('filter.alpha')}
                            />
                          </SliderField>
                          <div className="space-y-1.5">
                            <span className="text-[13px] text-ink">{t('filter.blendMode')}</span>
                            <div className="flex flex-wrap gap-1.5">
                              {FILTER_BLEND_MODES.map((blendMode) => (
                                <Chip
                                  key={blendMode}
                                  selected={filters.tint.blendMode === blendMode}
                                  onClick={() => updateTint({ blendMode })}
                                >
                                  {t(`filter.blend.${blendMode}`)}
                                </Chip>
                              ))}
                            </div>
                          </div>
                        </div>
                      </PanelSection>
                    )}
                  </AnimatePresence>
                </FieldGroup>
                <div className="flex justify-end">
                  <Button
                    size="sm"
                    disabled={!hasActiveFilters(filters)}
                    onClick={() => update({ filters: DEFAULT_FILTERS })}
                  >
                    {t('filter.reset')}
                  </Button>
                </div>
              </Section>

              <AnimatePresence initial={false}>
                {webtoon && (
                  <PanelSection>
                    <Section title={t('common:readingDirection.webtoon')}>
                      <FieldGroup>
                        <SliderField label={t('panel.sidePadding')} value={`${continuousPadding}%`}>
                          <Slider
                            value={continuousPadding}
                            onValueChange={(v) => updateGlobal({ continuousPadding: v })}
                            min={0}
                            max={40}
                            step={5}
                            label={t('panel.sidePadding')}
                          />
                        </SliderField>
                        <SliderField label={t('panel.pageGap')} value={`${continuousMargin}px`}>
                          <Slider
                            value={continuousMargin}
                            onValueChange={(v) => updateGlobal({ continuousMargin: v })}
                            min={0}
                            max={15}
                            step={5}
                            label={t('panel.pageGap')}
                          />
                        </SliderField>
                      </FieldGroup>
                    </Section>
                  </PanelSection>
                )}
              </AnimatePresence>
                  </div>
              </motion.aside>
            </RadixDialog.Content>
          </RadixDialog.Portal>
        )}
      </AnimatePresence>
    </RadixDialog.Root>
  )
}
