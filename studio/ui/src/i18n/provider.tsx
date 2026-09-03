import { type ReactNode, useCallback, useMemo } from "react";
import { IntlProvider, useIntl, type IntlShape } from "react-intl";
import type { MessageFormatElement } from "@formatjs/icu-messageformat-parser";

import enMessages from "./messages/en.json";

export type Locale = "en";

export const DEFAULT_LOCALE: Locale = "en";

const MESSAGE_CATALOG: Record<Locale, Record<string, string>> = {
  en: enMessages,
};

export interface I18nProviderProps {
  children: ReactNode;
  locale?: Locale;
}

export function I18nProvider({ children, locale = DEFAULT_LOCALE }: I18nProviderProps) {
  const messages = useMemo(
    () => MESSAGE_CATALOG[locale] as Record<string, string> | Record<string, MessageFormatElement[]>,
    [locale],
  );

  return (
    <IntlProvider locale={locale} messages={messages} defaultLocale={DEFAULT_LOCALE}>
      {children}
    </IntlProvider>
  );
}

export interface I18n {
  /** Translate a message by id. */
  t: (id: string, values?: Record<string, string | number>) => string;
  /** Current locale. */
  locale: Locale;
  /** Format a date value per locale. */
  formatDate: IntlShape["formatDate"];
  /** Format a number value per locale. */
  formatNumber: IntlShape["formatNumber"];
  /** Format a cost value per locale (currency-aware). */
  formatCost: (value: number, currency?: string) => string;
}

function buildI18n(intl: IntlShape, locale: Locale): I18n {
  return {
    t: (id, values) => intl.formatMessage({ id }, values),
    locale,
    formatDate: (value, opts) => intl.formatDate(value, opts),
    formatNumber: (value, opts) => intl.formatNumber(value, opts),
    formatCost: (value, currency = "USD") =>
      intl.formatNumber(value, {
        style: "currency",
        currency,
        minimumFractionDigits: 2,
        maximumFractionDigits: 4,
      }),
  };
}

/** Hook for translations and locale-aware formatting. */
export function useI18n(): I18n {
  const intl = useIntl();
  const locale = (intl.locale as Locale) ?? DEFAULT_LOCALE;

  // Memoize so consumers can use `i18n` in dependency arrays safely.
  return useMemo(() => buildI18n(intl, locale), [intl, locale]);
}

/** Shorthand hook when you only need the `t` function. */
export function useT(): (id: string, values?: Record<string, string | number>) => string {
  const intl = useIntl();
  return useCallback(
    (id: string, values?: Record<string, string | number>) => intl.formatMessage({ id }, values),
    [intl],
  );
}
