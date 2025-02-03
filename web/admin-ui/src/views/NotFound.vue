<template>
  <div class="flex items-center justify-center min-h-screen overflow-hidden mt-4">
    <div class="flex flex-col items-center justify-center">
      <div style="border-radius: 56px; padding: 0.3rem; background: linear-gradient(180deg, color-mix(in srgb, var(--primary-color), transparent 60%) 10%, var(--surface-ground) 30%)">
        <div class="w-full bg-surface-0 dark:bg-surface-900 py-16 px-6 sm:px-16 flex flex-col items-center" style="border-radius: 53px">
          <div class="flex justify-around align-center items-center">
            <img src="/img/bits-error.svg" alt="404" class="w-48 h-48 mb-4" />
            <h1 class="text-surface-700 dark:text-surface-200 font-bold text-4xl lg:text-6xl mb-2 ml-4">Arf!</h1>
          </div>
          <h2 class="text-surface-700 dark:text-surface-200 font-bold text-2xl lg:text-4xl mb-2">404: Not Found.</h2>
          <div class="text-surface-400 dark:text-surface-500 mb-8 text-xl">Looks like someone took a wrong turn.</div>
          <Button as="router-link" label="Go to Dashboard" to="/" />
        </div>
      </div>
    </div>
  </div>
</template>
