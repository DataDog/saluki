import { definePreset } from '@primevue/themes';
import Aura from '@primevue/themes/aura';

const DatadogTheme = definePreset(Aura, {
  semantic: {
    primary: {
      50: '#f2ecfc',
      100: '#f2ecfc',
      200: '#e6d6f9',
      300: '#e6d6f9',
      400: '#cbb1ed',
      500: '#b48fe1',
      600: '#9364cd',
      700: '#632ca6',
      800: '#632ca6',
      900: '#451481',
      950: '#451481'
    }
  }
});

export default DatadogTheme;
