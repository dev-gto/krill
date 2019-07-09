let messages = {
  en: EN_I18N,
  it: IT_I18N
}

// Create VueI18n instance with options
const i18n = new VueI18n({
  locale: 'en', // set locale
  messages, // set locale messages
})