import App from './App.vue';
import router from './router';
import DatadogTheme from './theme';

import { createApp } from 'vue';
import { createPinia } from 'pinia';
import PrimeVue from 'primevue/config';
import VueApexCharts from 'vue3-apexcharts';
import ConfirmationService from 'primevue/confirmationservice';
import ToastService from 'primevue/toastservice';
import { createGrpcWebTransport } from '@connectrpc/connect-web';

import '@/assets/styles.scss';
import '@/assets/tailwind.css';

const app = createApp(App);
const pinia = createPinia();

app.use(router);
app.use(pinia);
app.use(PrimeVue, {
  theme: {
    preset: DatadogTheme,
    options: {
      darkModeSelector: '.app-dark'
    }
  }
});
app.use(ToastService);
app.use(ConfirmationService);
app.use(VueApexCharts);

const transport = createGrpcWebTransport({
  baseUrl: '/api'
});
app.provide('grpc-api-transport', transport);

app.mount('#app');
