const Login = {
  template: `
  <div class="login">
    <el-row type="flex" justify="center">
      <el-col :span="10">
        <el-card class="box-card">
          <div class="text item">
            <el-form :model="form" :inline="true" ref="loginForm">
              <el-form-item
                :label="$t('login.password')"
                prop="token"
                :rules="[{ required: true, message: $t('login.required')}]"
              >
                <el-input
                  type="password"
                  :placeholder="$t('login.placeholder')"
                  v-model="form.token"
                  clearable
                  @keyup.enter.native="submitForm"
                ></el-input>
              </el-form-item>
              <el-form-item>
                <el-button type="primary" @click="submitForm" :loading="loading">{{ $t("login.signin") }}</el-button>
              </el-form-item>
            </el-form>
          </div>
        </el-card>
      </el-col>
    </el-row>
    <el-row type="flex" class="alert-row" justify="center">
      <el-col :span="10">
        <el-alert type="error" v-if="error" :closable="false">{{error}}</el-alert>
      </el-col>
    </el-row>
    <div class="route-left">
      <img src="assets/images/route_left.svg">
    </div>
    <div class="route-right">
      <img src="assets/images/route_right.svg">
    </div>
  </div>
  
  `,
  data() {
    return {
      form: {
        token: ""
      },
      submitted: false,
      loading: false,
      returnUrl: "",
      error: ""
    };
  },
  created() {
    this.returnUrl = this.$route.query.returnUrl || "/";
  },
  methods: {
    login() {
      this.submitted = true;

      const self = this;
      this.loading = true;
      APIService.login(this.form.token)
        .then(() => {
          this.$emit("authEvent");
          router.push(this.returnUrl);
        })
        .catch(function(error) {
          self.error = error;
          self.loading = false;
        });
    },
    submitForm() {
      this.$refs["loginForm"].validate(valid => {
        if (valid) {
          this.login();
        } else {
          return false;
        }
      });
    }
  }
}